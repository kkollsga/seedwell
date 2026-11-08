//! A tracker's Info page (SPEC 7E) as a view model every host draws (docs/CROSS-PLATFORM.md §1): the way back, the
//! title, the Remove from Favorites question, the header's numbers, what the tracker is doing now, peer exchange for a
//! private one, a favorite's speed limits, folders and addresses (edited in place, `favorite_editor.rs`), and for an
//! automatic tracker how to get them. Rust decides every row and word; the host reads the tracker
//! ([`Core::tracker_info_read`], every `refresh_ms`, off the main thread), builds the page
//! ([`FavoriteEditor::tracker_info_page`]) and runs its [`TrackerInfoCommand`]s ([`FavoriteEditor::tracker_info_run`]).
//!
//! - [`tracker_header_model`]: the header alone, also over a tracker's list of torrents.

use crate::display::{rate_with_limit_line, RateDirection};
use crate::favorite_editor::{same_folders, FavoriteEditor, FavoriteEffect, FavoriteRemoval, FavoriteTarget};
use crate::host::HostWindow;
use crate::settings::{ProfileRecord, VolumeWarning};
use crate::types::{DailyStat, RowQuery, TrackerLine, TrackerStats, TrackerSummary, TransferKind, TransferRow};
use crate::Core;
use seedwell_control::display as d;

/// How often the host reads the tracker again while its page is shown (and when its torrent count changes).
const REFRESH_MS: u32 = 10_000;
/// How many of its torrents' announce lines tell the tracker's health (Get Info already has them).
const SAMPLED_TORRENTS: usize = 4;
const ADDRESS_PROMPT: &str = "tracker.example.org or a full announce URL";

// MARK: What the host reads

/// The busiest of the tracker's torrents now.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct TrackerBusiest {
    pub name: String,
    pub up_rate: u64,
}

/// What the page shows of the tracker, read every `refresh_ms` ([`Core::tracker_info_read`]).
#[derive(Clone, Debug, uniffi::Record)]
pub struct TrackerInfoRead {
    pub tracker_id: String,
    /// An automatic tracker's announce hosts (a favorite's addresses are its own).
    pub hosts: Vec<String>,
    pub sharing: u32,
    pub downloading: u32,
    pub busiest: Option<TrackerBusiest>,
    /// Unix seconds of the newest successful announce of the sampled torrents.
    pub last_announce: Option<u64>,
    /// The mean gap between the last and the next announce of the lines that have both, seconds.
    pub interval_secs: Option<u64>,
    /// The first failure ("failed: …"), if a line failed.
    pub problem: Option<String>,
    /// Announce lines looked at.
    pub sampled: u32,
    /// Peer exchange allowed for this tracker, and the master switch in Settings › Network.
    pub pex_allowed: bool,
    pub pex_master: bool,
    pub stats: TrackerStats,
    /// The last 30 days, for the header.
    pub daily: Vec<DailyStat>,
}

/// The tracker's health from some of its torrents' announce lines.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Facts {
    pub last_announce: Option<u64>,
    pub interval_secs: Option<u64>,
    pub problem: Option<String>,
    pub sampled: u32,
}

/// `lines` are the tracker lines of the sampled torrents that belong to this tracker.
pub(crate) fn summarize(lines: &[TrackerLine]) -> Facts {
    let mut f = Facts { sampled: u32::try_from(lines.len()).unwrap_or(u32::MAX), ..Facts::default() };
    let mut gaps: Vec<u64> = Vec::new();
    for l in lines {
        let status = l.status.to_lowercase();
        if status.starts_with("failed") || status.contains("error") {
            if f.problem.is_none() {
                f.problem = Some(l.status.clone());
            }
        } else if let Some(last) = l.last_announce {
            f.last_announce = Some(f.last_announce.unwrap_or(0).max(last));
        }
        if let (Some(last), Some(next)) = (l.last_announce, l.next_announce)
            && next > last
        {
            gaps.push(next - last);
        }
    }
    if !gaps.is_empty() {
        f.interval_secs = Some(gaps.iter().sum::<u64>() / gaps.len() as u64);
    }
    f
}

/// The host of a (masked) announce URL, lower case.
pub(crate) fn host_of(url: &str) -> String {
    let s = url.split_once("://").map_or(url, |(_, rest)| rest);
    let end = s.find(['/', '?', '#']).unwrap_or(s.len());
    let h = s[..end].to_lowercase();
    if let Some(rest) = h.strip_prefix('[')
        && let Some(close) = rest.find(']')
    {
        return rest[..close].to_string();
    }
    match h.rsplit_once(':') {
        Some((host, _)) if h.matches(':').count() == 1 => host.to_string(),
        _ => h,
    }
}

/// Whether an announce URL belongs to the tracker: its host is one of `hosts` (a `*.` pattern matches subdomains), or
/// lies under the automatic tracker's `domain`.
pub(crate) fn belongs(url: &str, hosts: &[String], domain: Option<&str>) -> bool {
    let h = host_of(url);
    if let Some(d) = domain
        && (h == d || h.ends_with(&format!(".{d}")))
    {
        return true;
    }
    hosts.iter().any(|p| {
        let ph = host_of(&if p.contains("://") { p.clone() } else { format!("x://{p}") });
        match ph.strip_prefix('*') {
            Some(suffix) if ph.starts_with("*.") => h.ends_with(suffix),
            _ => ph == h,
        }
    })
}

/// "45 s", "30 min", "2 h", "3 days".
pub(crate) fn span(s: u64) -> String {
    if s < 90 {
        format!("{s} s")
    } else if s < 5400 {
        format!("{} min", (s + 30) / 60)
    } else if s < 172_800 {
        format!("{} h", (s + 1800) / 3600)
    } else {
        format!("{} days", (s + 43_200) / 86_400)
    }
}

/// "just now", "3 min ago".
pub(crate) fn ago(unix: u64, now: u64) -> String {
    if now <= unix { "just now".into() } else { format!("{} ago", span(now - unix)) }
}

impl Core {
    /// What a tracker's Info page shows of it (its torrents, their announce lines, peer exchange, the header's numbers).
    /// Reads a few torrents' details: off the main thread, every `TrackerInfoPageModel::refresh_ms`.
    pub fn tracker_info_read(&self, tracker: TrackerSummary) -> TrackerInfoRead {
        let id = tracker.id.clone();
        let hosts = if tracker.favorite { Vec::new() } else { self.backend.tracker_hosts(&id) };
        let urls = if tracker.favorite {
            self.profiles_load().into_iter().find(|p| p.id == id).map(|p| p.urls).unwrap_or_default()
        } else {
            hosts.clone()
        };
        let snap = self.backend.snapshot();
        let moving: Vec<TransferRow> =
            [&snap.downloading, &snap.hidden_downloading, &snap.queued, &snap.hidden_queued].into_iter().flatten().cloned().collect();
        let downloading = moving.iter().filter(|r| r.tracker_id == id).count();
        let sharing = self.backend.sharing_counts(Some(&id), None, "").total();
        let query = RowQuery { group: None, tracker_id: Some(id.clone()), text: String::new(), category: None, offset: 0, limit: 200 };
        let mine: Vec<TransferRow> =
            moving.into_iter().chain(self.backend.rows(&query)).filter(|r| r.tracker_id == id && r.kind == TransferKind::Torrent).collect();
        let busiest = mine.iter().reduce(|a, b| if b.up_rate > a.up_rate { b } else { a });
        let domain = (!tracker.favorite).then_some(id.as_str());
        let urls: &[String] = &urls;
        let lines: Vec<TrackerLine> = mine
            .iter()
            .take(SAMPLED_TORRENTS)
            .filter_map(|r| self.backend.info(&r.id).ok())
            .flat_map(|info| info.trackers.into_iter().filter(move |l| belongs(&l.url, urls, domain)))
            .collect();
        let f = summarize(&lines);
        TrackerInfoRead {
            tracker_id: id.clone(),
            hosts,
            sharing,
            downloading: u32::try_from(downloading).unwrap_or(u32::MAX),
            busiest: busiest.map(|b| TrackerBusiest { name: b.display_name.clone(), up_rate: b.up_rate }),
            last_announce: f.last_announce,
            interval_secs: f.interval_secs,
            problem: f.problem,
            sampled: f.sampled,
            pex_allowed: self.settings.tracker_pex(&id),
            pex_master: self.settings_load().network.pex,
            stats: self.backend.tracker_stats(&id),
            daily: self.backend.tracker_daily(&id, 30),
        }
    }
}

#[uniffi::export]
impl Core {
}

// MARK: The header

/// One number of the header: "26 GB", "Uploaded", "Last 7 days".
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct TrackerHeaderStat {
    pub value: String,
    pub caption: String,
    /// The grey line under it; `None`: an empty line (the numbers stay aligned).
    pub sub: Option<String>,
}

/// One day of the sparkline: uploaded bytes, `days_ago` 0 being today.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Record)]
pub struct TrackerHeaderPoint {
    pub days_ago: u32,
    pub up_bytes: u64,
}

/// The summary above a tracker's torrents and on its Info page (SPEC 7E.2): the speeds now, the last 7 days' upload,
/// download and ratio, the completed count and, when there was upload, a small sparkline of the last 30 days without
/// axes or dates (the caption says what it is).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct TrackerHeaderModel {
    pub stats: Vec<TrackerHeaderStat>,
    /// Empty: no sparkline.
    pub chart: Vec<TrackerHeaderPoint>,
    pub chart_caption: String,
}

/// The header of a tracker: its totals, its days, and its torrents' rates now (bytes/s).
#[uniffi::export]
pub fn tracker_header_model(stats: TrackerStats, daily: Vec<DailyStat>, down: u64, up: u64) -> TrackerHeaderModel {
    let week = daily.iter().filter(|x| x.days_ago < 7);
    let (wup, wdown) = week.fold((0u64, 0u64), |(u, dn), x| (u + x.up_bytes, dn + x.down_bytes));
    let ratio = if wdown == 0 { 0.0 } else { wup as f64 / wdown as f64 };
    let stat = |value: String, caption: &str, sub: Option<&str>| TrackerHeaderStat { value, caption: caption.into(), sub: sub.map(Into::into) };
    let week_sub = Some("Last 7 days");
    let traffic = daily.iter().any(|x| x.up_bytes > 0);
    TrackerHeaderModel {
        stats: vec![
            stat(d::live_rate(down, "↓"), "Download speed", None),
            stat(d::live_rate(up, "↑"), "Upload speed", None),
            stat(d::bytes(wup), "Uploaded", week_sub),
            stat(d::bytes(wdown), "Downloaded", week_sub),
            stat(d::ratio(ratio), "Ratio", week_sub),
            stat(stats.torrents_completed.to_string(), "Torrents completed", None),
        ],
        chart: if traffic { daily.iter().map(|x| TrackerHeaderPoint { days_ago: x.days_ago, up_bytes: x.up_bytes }).collect() } else { Vec::new() },
        chart_caption: "Upload, last 30 days".into(),
    }
}

// MARK: The page

/// What an item of the page does.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum TrackerInfoCommand {
    /// Back to the tracker's torrents: the host's `Library::set_tracker_info(false)`.
    Back,
    /// The peer exchange switch.
    SetPex { allow: bool },
    /// The pen of an address: the line becomes a field.
    EditAddress { index: u32 },
    /// The x of an address (the last one asks first).
    RemoveAddress { index: u32 },
    /// The "+": a new, empty line.
    AddAddress,
    /// The changed folders: back to the saved ones, or saved.
    RevertFolders,
    SaveFolders,
    /// The Remove from Favorites question's two buttons.
    CancelRemoval,
    ConfirmRemoval,
}

/// What a button looks like: words, or a symbol (with its title as the tooltip and the screen reader's label).
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum TrackerInfoSymbol {
    /// "‹ Torrents": a chevron before the title.
    Back,
    /// A pen.
    Edit,
    /// An x.
    Remove,
    /// A plus.
    Add,
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct TrackerInfoButton {
    pub title: String,
    pub help: String,
    pub symbol: Option<TrackerInfoSymbol>,
    /// The default button (Save): drawn prominent.
    pub prominent: bool,
    pub command: TrackerInfoCommand,
}

/// A line of "Now": a label and its value (in the error colour when `error`).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct TrackerInfoFact {
    pub label: String,
    pub value: String,
    pub error: bool,
}

/// Peer exchange for a private tracker's torrents: off unless the user allows it, since most private trackers forbid it
/// (SPEC 7B.4). The master switch in Settings › Network overrides it.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct TrackerInfoPex {
    pub title: String,
    pub label: String,
    pub on: bool,
    pub enabled: bool,
    pub help: String,
    pub note: String,
    /// The note says why the switch is off (drawn a little stronger than the usual quiet note).
    pub note_is_reason: bool,
}

/// A speed limit of a favorite: its label, the limit (bytes/s, 0 = unlimited; the host's rate field edits it and calls
/// `FavoriteEditor::set_limits` with both), and the rate now against it.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct TrackerInfoLimit {
    pub label: String,
    pub direction: RateDirection,
    pub limit: u64,
    pub line: String,
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct TrackerInfoLimits {
    pub title: String,
    /// Empty for an automatic tracker.
    pub rows: Vec<TrackerInfoLimit>,
    /// The quiet note under them (an automatic tracker: how to get limits).
    pub note: String,
}

/// A favorite's own folders (SPEC 7A.4): the record the host's folder editor edits (the draft while changed: every
/// change goes to `FavoriteEditor::set_folder_draft`), its different-disk warnings, Revert and Save while changed.
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct TrackerInfoLocation {
    pub title: String,
    /// `None` for an automatic tracker: `note` says how to get folders.
    pub record: Option<ProfileRecord>,
    pub warnings: Vec<VolumeWarning>,
    pub buttons: Vec<TrackerInfoButton>,
    /// A failed save (red).
    pub hint: Option<String>,
    pub note: Option<String>,
}

/// The field of an address being edited (Return commits, Escape cancels, leaving it commits).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct TrackerInfoField {
    pub target: FavoriteTarget,
    pub prompt: String,
    pub text: String,
    /// Red, under it.
    pub hint: Option<String>,
}

/// An address: its text (cut in the middle, selectable), or its field while edited, and its buttons (a favorite's).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct TrackerInfoAddress {
    pub text: String,
    pub field: Option<TrackerInfoField>,
    pub buttons: Vec<TrackerInfoButton>,
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct TrackerInfoAddresses {
    pub title: String,
    pub rows: Vec<TrackerInfoAddress>,
    /// A new line being typed, under the list.
    pub adding: Option<TrackerInfoField>,
    /// The "+" (a favorite's).
    pub add: Option<TrackerInfoButton>,
    /// Quiet, under them: the note after a save, or how to edit an automatic tracker's.
    pub note: Option<String>,
}

/// The page, top to bottom. Section titles are drawn in small capitals.
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct TrackerInfoPageModel {
    pub tracker_id: String,
    pub back: TrackerInfoButton,
    pub title: String,
    /// A favorite: a star before the title.
    pub favorite: bool,
    /// "Info", quiet, after the title.
    pub subtitle: String,
    /// The Remove from Favorites question, while it is open for this tracker: Cancel runs `CancelRemoval`, Remove
    /// `ConfirmRemoval`.
    pub removal: Option<FavoriteRemoval>,
    /// `None` until the first read.
    pub header: Option<TrackerHeaderModel>,
    pub now_title: String,
    pub now: Vec<TrackerInfoFact>,
    /// A private tracker's.
    pub pex: Option<TrackerInfoPex>,
    pub limits: TrackerInfoLimits,
    pub location: TrackerInfoLocation,
    pub addresses: TrackerInfoAddresses,
    /// How often the host reads the tracker again ([`Core::tracker_info_read`]).
    pub refresh_ms: u32,
}

/// What the page is built from.
#[derive(Clone, Debug, uniffi::Record)]
pub struct TrackerInfoInput {
    pub tracker: TrackerSummary,
    /// The last read (`None` before the first).
    pub read: Option<TrackerInfoRead>,
    /// The tracker's torrents moving now, bytes/s (the library's `tracker_rates`).
    pub down: u64,
    pub up: u64,
    pub now_unix_s: u64,
    /// The host's windows: without `FavoriteEditing` a favorite's limits, folders and addresses are shown, not edited.
    pub host: Vec<HostWindow>,
}

fn button(title: &str, help: &str, symbol: Option<TrackerInfoSymbol>, command: TrackerInfoCommand) -> TrackerInfoButton {
    TrackerInfoButton { title: title.into(), help: help.into(), symbol, prominent: false, command }
}

fn fact(label: &str, value: String, error: bool) -> TrackerInfoFact {
    TrackerInfoFact { label: label.into(), value, error }
}

fn now_facts(read: Option<&TrackerInfoRead>, now: u64) -> Vec<TrackerInfoFact> {
    let (sharing, downloading) = read.map_or((0, 0), |r| (r.sharing, r.downloading));
    let mut out = vec![fact("Torrents", format!("{sharing} sharing · {downloading} downloading"), false)];
    if let Some(b) = read.and_then(|r| r.busiest.as_ref()).filter(|b| b.up_rate > 0) {
        out.push(fact("Busiest now", format!("{} · ↑ {}", b.name, d::rate(b.up_rate)), false));
    }
    let problem = read.and_then(|r| r.problem.clone());
    let state = match &problem {
        Some(p) => p.clone(),
        None if read.is_none_or(|r| r.sampled == 0) => "Not announced yet".into(),
        None => "Working".into(),
    };
    out.push(fact("Tracker", state, problem.is_some()));
    if let Some(t) = read.and_then(|r| r.last_announce) {
        out.push(fact("Last announce", ago(t, now), false));
    }
    if let Some(i) = read.and_then(|r| r.interval_secs) {
        out.push(fact("Announce interval", format!("about {}", span(i)), false));
    }
    out
}

fn pex(tracker: &TrackerSummary, read: Option<&TrackerInfoRead>) -> Option<TrackerInfoPex> {
    if !tracker.private {
        return None;
    }
    let (on, master) = read.map_or((false, true), |r| (r.pex_allowed, r.pex_master));
    const OFF: &str = "Peer exchange is off in Settings › Network";
    Some(TrackerInfoPex {
        title: "Peer exchange".into(),
        label: "Peer exchange (PEX)".into(),
        on,
        enabled: master,
        help: if !master {
            OFF.into()
        } else if on {
            "On, click to turn off".into()
        } else {
            "Off, click to turn on".into()
        },
        note: if master { "Only turn this on if the tracker's rules allow peer exchange. Most private trackers forbid it.".into() } else { OFF.into() },
        note_is_reason: !master,
    })
}

#[uniffi::export]
impl FavoriteEditor {
    /// The tracker's Info page as it is now.
    pub fn tracker_info_page(&self, input: TrackerInfoInput) -> TrackerInfoPageModel {
        let st = self.lock();
        let t = &input.tracker;
        let read = input.read.as_ref().filter(|r| r.tracker_id == t.id);
        let record = st.record(&t.id).filter(|_| t.favorite);
        let editable = record.is_some() && input.host.contains(&HostWindow::FavoriteEditing);
        let field = |target: FavoriteTarget| TrackerInfoField { target, prompt: ADDRESS_PROMPT.into(), text: st.text.clone(), hint: st.hint.clone() };

        let limits = match record {
            Some(r) => TrackerInfoLimits {
                title: "Speed limits".into(),
                rows: [("Download", RateDirection::Download, r.down_limit, input.down), ("Upload", RateDirection::Upload, r.up_limit, input.up)]
                    .into_iter()
                    .map(|(label, direction, limit, now)| TrackerInfoLimit {
                        label: label.into(),
                        direction,
                        limit: limit.unwrap_or(0),
                        line: rate_with_limit_line(now, limit),
                    })
                    .collect(),
                note: format!("One cap for all torrents on {} together. The global limits still apply; the lower one wins.", t.name),
            },
            None => TrackerInfoLimits { title: "Speed limits".into(), rows: Vec::new(), note: "Save as Favorite to set limits".into() },
        };

        let location = match record {
            Some(saved) => {
                let draft = st.folders.as_ref().filter(|(id, _)| *id == t.id).map(|(_, r)| r);
                let shown = draft.unwrap_or(saved).clone();
                let changed = draft.is_some_and(|dr| !same_folders(dr, saved));
                let mut save = button("Save", "Use these folders for this tracker", None, TrackerInfoCommand::SaveFolders);
                save.prominent = true;
                TrackerInfoLocation {
                    title: "Download location".into(),
                    warnings: self.store.volume_warnings(&shown),
                    record: Some(shown),
                    buttons: if changed { vec![button("Revert", "Discard the changed folders", None, TrackerInfoCommand::RevertFolders), save] } else { Vec::new() },
                    // A failed save of the folders or limits; an address field shows its own hint.
                    hint: st.hint.clone().filter(|_| st.target.is_none()),
                    note: None,
                }
            }
            None => TrackerInfoLocation {
                title: "Download location".into(),
                record: None,
                warnings: Vec::new(),
                buttons: Vec::new(),
                hint: None,
                note: Some("Save as Favorite to choose folders".into()),
            },
        };

        let urls: Vec<String> = match record {
            Some(r) => r.urls.clone(),
            None => read.map(|r| r.hosts.clone()).unwrap_or_default(),
        };
        let only_one = urls.len() == 1;
        let rows = urls
            .into_iter()
            .enumerate()
            .map(|(i, text)| {
                let index = u32::try_from(i).unwrap_or(u32::MAX);
                let target = FavoriteTarget::Url { tracker_id: t.id.clone(), index: Some(index) };
                let editing = editable && st.target.as_ref() == Some(&target);
                TrackerInfoAddress {
                    text,
                    field: editing.then(|| field(target)),
                    buttons: if editable && !editing {
                        vec![
                            button("Edit this address", "Edit this address", Some(TrackerInfoSymbol::Edit), TrackerInfoCommand::EditAddress { index }),
                            {
                                let help = if only_one {
                                    "Remove this address (it is the only one: removes the favorite)"
                                } else {
                                    "Remove this address"
                                };
                                button(help, help, Some(TrackerInfoSymbol::Remove), TrackerInfoCommand::RemoveAddress { index })
                            },
                        ]
                    } else {
                        Vec::new()
                    },
                }
            })
            .collect();
        let new_line = FavoriteTarget::Url { tracker_id: t.id.clone(), index: None };
        let addresses = TrackerInfoAddresses {
            title: "Tracker URLs".into(),
            rows,
            adding: (editable && st.target.as_ref() == Some(&new_line)).then(|| field(new_line)),
            add: editable.then(|| button("Add a tracker address", "Add a tracker address", Some(TrackerInfoSymbol::Add), TrackerInfoCommand::AddAddress)),
            note: if record.is_some() { st.note().map(str::to_owned) } else { Some("Save as Favorite to edit".into()) },
        };

        TrackerInfoPageModel {
            tracker_id: t.id.clone(),
            back: button("Torrents", "Back to this tracker's torrents", Some(TrackerInfoSymbol::Back), TrackerInfoCommand::Back),
            title: t.name.clone(),
            favorite: t.favorite,
            subtitle: "Info".into(),
            removal: st.removal.clone().filter(|q| q.id == t.id),
            header: read.map(|r| tracker_header_model(r.stats.clone(), r.daily.clone(), input.down, input.up)),
            now_title: "Now".into(),
            now: now_facts(read, input.now_unix_s),
            pex: pex(t, read),
            limits,
            location,
            addresses,
            refresh_ms: REFRESH_MS,
        }
    }

    /// Runs a command of `tracker_id`'s Info page (all but `Back`, which is the host's), after the saves started
    /// before it; a command that saves (peer exchange, an address removed, the folders, a removal) saves off the
    /// caller's thread.
    #[uniffi::method(name = "tracker_info_run")]
    pub async fn tracker_info_run_async(self: std::sync::Arc<Self>, tracker_id: String, command: TrackerInfoCommand) -> FavoriteEffect {
        let me = self.clone();
        self.in_order(move |_| me.tracker_info_run(tracker_id, command)).await
    }
}

impl FavoriteEditor {
    /// Runs a command of `tracker_id`'s Info page (all but `Back`, which is the host's).
    pub(crate) fn tracker_info_run(&self, tracker_id: String, command: TrackerInfoCommand) -> FavoriteEffect {
        match command {
            TrackerInfoCommand::Back => FavoriteEffect::default(),
            TrackerInfoCommand::SetPex { allow } => {
                if let Err(e) = self.store.set_pex(&tracker_id, allow) {
                    self.lock().hint = Some(e.to_string());
                }
                FavoriteEffect::default()
            }
            TrackerInfoCommand::EditAddress { index } => {
                self.begin_edit_url(tracker_id, index);
                FavoriteEffect::default()
            }
            TrackerInfoCommand::RemoveAddress { index } => self.remove_url(tracker_id, index),
            TrackerInfoCommand::AddAddress => {
                self.begin_add_url(tracker_id);
                FavoriteEffect::default()
            }
            TrackerInfoCommand::RevertFolders => {
                self.revert_folders();
                FavoriteEffect::default()
            }
            TrackerInfoCommand::SaveFolders => self.save_folders(tracker_id),
            TrackerInfoCommand::CancelRemoval => {
                self.cancel_removal();
                FavoriteEffect::default()
            }
            TrackerInfoCommand::ConfirmRemoval => self.confirm_removal(),
        }
    }
}

#[cfg(test)]
#[path = "tracker_page_tests.rs"]
mod tests;
