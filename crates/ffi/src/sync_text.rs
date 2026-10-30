//! Folder sync's words and small decisions (SPEC 7G, 8.2 item 5) that the menu, the window, the modals and the file
//! manager's badges share (docs/CROSS-PLATFORM.md §1): a folder's grey line and status image, roles and what they may
//! do, the members' order and lines, pasted links, invitation links and the sharing period, the add and edit modals'
//! rules, the Share dialog's choices, and the sentences of errors and alerts. A text that names this computer says
//! what the system calls it ("Mac", "PC": `trs_platform::computer!`).
//!
//! A date in the user's own style ("1 Nov", "1. nov.") stays the host's, like the time of day: the functions that show
//! one take it already written (`…_day`).

use std::path::{Path, PathBuf};

use trs_control::display as d;
use trs_platform::fs::TrashError;
use trs_platform::{computer, file_manager, trash};

use crate::files::standard_order;
use crate::sync::{SyncAccess, SyncDevice, SyncFolder, SyncInvite, SyncLinkInfo, SyncMember, SyncRole, SyncState};
use crate::types::CoreError;

/// Longest folder name the menu shows before the middle is cut.
pub(crate) const TITLE_LIMIT: usize = 40;
/// Active links the Share dialog lists before "and N more".
const LINKS_SHOWN: usize = 6;
const DAY_S: u64 = 86_400;

/// Swift's `.whitespaces`, which the app trimmed with: spaces and tabs, not line breaks.
fn trim_spaces(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() && !matches!(c, '\n' | '\r' | '\u{0B}' | '\u{0C}' | '\u{85}' | '\u{2028}' | '\u{2029}'))
}

// MARK: A folder

/// How far the folder is (0...1): the bytes it has of the bytes it should have. `None` when not known.
pub(crate) fn fraction(f: &SyncFolder) -> Option<f64> {
    (f.bytes > 0 && f.need_bytes <= f.bytes).then(|| (f.bytes - f.need_bytes) as f64 / f.bytes as f64)
}

fn devices(n: u32) -> String {
    d::count(u64::from(n), "device")
}

pub(crate) fn subtitle(f: &SyncFolder) -> String {
    let conflicts = (f.conflicts > 0).then(|| d::count(u64::from(f.conflicts), "conflict"));
    let after_conflicts = |parts: Vec<String>| conflicts.iter().cloned().chain(parts).collect::<Vec<_>>().join(" · ");
    match f.state {
        SyncState::Missing => "Folder missing".into(),
        SyncState::Paused => "Paused".into(),
        SyncState::Error => f.error.as_ref().map_or_else(|| "Can't sync".into(), |e| format!("Can't sync · {e}")),
        SyncState::Starting => "Starting…".into(),
        SyncState::Scanning => after_conflicts(vec!["Scanning…".into()]),
        SyncState::WaitingForDevices => after_conflicts(vec!["Waiting for devices".into()]),
        SyncState::WaitingForAdmin => "Waiting for the owner or an admin".into(),
        SyncState::Syncing => {
            let mut parts = vec!["Syncing".to_string()];
            if let Some(p) = fraction(f) {
                // Never 100 % while it syncs.
                parts.push(format!("{}%", ((p * 100.0) as u64).min(99)));
            } else if f.need_files > 0 {
                parts.push(format!("{} to go", d::count(f.need_files, "file")));
            }
            if f.rate_in > 0 {
                parts.push(d::rate(f.rate_in));
            }
            after_conflicts(parts)
        }
        SyncState::UpToDate => match (conflicts, f.connected) {
            (Some(c), 0) => c,
            (Some(c), n) => format!("{c} · {}", devices(n)),
            (None, 0) => "Up to date".into(),
            (None, n) => format!("Up to date · {}", devices(n)),
        },
    }
}

/// The grey line under a folder's name: "Up to date · 3 devices", "Syncing · 42% · 12 MB/s", "Waiting for devices",
/// "Folder missing", "1 conflict · 2 devices".
#[uniffi::export]
pub fn sync_folder_subtitle(folder: SyncFolder) -> String {
    subtitle(&folder)
}

/// The image at the left of a folder (the menu's rows, the window's list and page).
#[derive(Clone, Copy, Debug, PartialEq, uniffi::Enum)]
pub enum SyncBadge {
    /// A green check.
    UpToDate,
    /// A progress ring, 0...1 (the host draws at least a sliver).
    Progress { fraction: f64 },
    /// Waiting for another device (to come online, or to admit this one): nothing moves until it does. A clock.
    Waiting,
    Paused,
    /// A conflict copy or an error: something for the user to look at.
    Attention,
    Missing,
}

pub(crate) fn badge(f: &SyncFolder) -> SyncBadge {
    match f.state {
        SyncState::Missing => SyncBadge::Missing,
        SyncState::Paused => SyncBadge::Paused,
        SyncState::Error => SyncBadge::Attention,
        _ if f.conflicts > 0 => SyncBadge::Attention,
        SyncState::Syncing => SyncBadge::Progress { fraction: fraction(f).unwrap_or(0.0) },
        // Waiting is not progress: a ring would read as "almost done" (or done) while nothing moves (the user,
        // 2026-10-28). The subtitle says what is still to fetch.
        SyncState::WaitingForDevices | SyncState::WaitingForAdmin => SyncBadge::Waiting,
        SyncState::Starting | SyncState::Scanning => SyncBadge::Progress { fraction: 0.0 },
        SyncState::UpToDate => SyncBadge::UpToDate,
    }
}

#[uniffi::export]
pub fn sync_folder_badge(folder: SyncFolder) -> SyncBadge {
    badge(&folder)
}

/// The badge in words, for a screen reader: "up to date", "syncing", "needs attention".
#[uniffi::export]
pub fn sync_badge_label(badge: SyncBadge) -> String {
    match badge {
        SyncBadge::UpToDate => "up to date",
        SyncBadge::Progress { .. } => "syncing",
        SyncBadge::Waiting => "waiting",
        SyncBadge::Paused => "paused",
        SyncBadge::Attention => "needs attention",
        SyncBadge::Missing => "folder missing",
    }
    .into()
}

/// The badge of a synced folder and its files in the file manager (Finder's sync badges, Explorer's overlays).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum SyncFileBadge {
    Synced,
    Syncing,
    Downloading,
    Uploading,
    Paused,
    /// No device to sync with is online.
    Offline,
    Conflict,
    Error,
}

/// A folder's own file-manager badge. A conflict waits for the user: it shows unless something is moving right now.
#[uniffi::export]
pub fn sync_file_badge(folder: SyncFolder) -> SyncFileBadge {
    use SyncFileBadge as B;
    let f = &folder;
    let b = match f.state {
        SyncState::Paused => return B::Paused,
        SyncState::Missing | SyncState::Error => return B::Error,
        SyncState::Starting | SyncState::Scanning => B::Syncing,
        SyncState::WaitingForDevices | SyncState::WaitingForAdmin => B::Offline,
        SyncState::Syncing => match (f.rate_in > 0 || f.need_files > 0, f.rate_out > 0) {
            (true, false) => B::Downloading,
            (false, true) => B::Uploading,
            _ => B::Syncing,
        },
        SyncState::UpToDate if f.rate_out > 0 => B::Uploading,
        SyncState::UpToDate if f.connected == 0 => B::Offline,
        SyncState::UpToDate => B::Synced,
    };
    if f.conflicts > 0 && matches!(b, B::Synced | B::Uploading | B::Offline) { B::Conflict } else { b }
}

// MARK: Roles and members

/// "Read & write", "Read only", "Encrypted".
#[uniffi::export]
pub fn sync_access_label(access: SyncAccess) -> String {
    match access {
        SyncAccess::ReadWrite => "Read & write",
        SyncAccess::ReadOnly => "Read only",
        SyncAccess::Encrypted => "Encrypted",
    }
    .into()
}

pub(crate) fn role(r: SyncRole) -> &'static str {
    match r {
        SyncRole::Owner => "Owner",
        SyncRole::Admin => "Admin",
        SyncRole::ReadWrite => "Read & write",
        SyncRole::ReadOnly => "Read only",
    }
}

/// A role as people read it: "Owner", "Admin", "Read & write", "Read only".
#[uniffi::export]
pub fn sync_role_label(role: SyncRole) -> String {
    self::role(role).into()
}

/// What a role may do, in one line (the Share dialog and the members' menu).
#[uniffi::export]
pub fn sync_role_note(role: SyncRole) -> String {
    match role {
        SyncRole::Owner => "Can do everything, including changing roles",
        SyncRole::Admin => "Can change files and share the folder",
        SyncRole::ReadWrite => "Can change files",
        SyncRole::ReadOnly => "Receives changes, never sends its own",
    }
    .into()
}

/// The device may share the folder or channel (make invitation links): owners and admins.
#[uniffi::export]
pub fn sync_can_share(role: SyncRole) -> bool {
    matches!(role, SyncRole::Owner | SyncRole::Admin)
}

fn rank(r: SyncRole) -> u8 {
    match r {
        SyncRole::Owner => 0,
        SyncRole::Admin => 1,
        SyncRole::ReadWrite => 2,
        SyncRole::ReadOnly => 3,
    }
}

/// Members in the order a folder or channel page lists them: this computer first, then by role (owners first), then
/// by name (the file manager's order: case aside, numbers by value).
#[uniffi::export]
pub fn sync_members_ordered(members: Vec<SyncMember>) -> Vec<SyncMember> {
    let mut m = members;
    m.sort_by(|a, b| b.is_me.cmp(&a.is_me).then(rank(a.role).cmp(&rank(b.role))).then_with(|| standard_order(&a.name, &b.name)));
    m
}

/// How a device is reached: "Local network", "Through a relay", "Internet".
fn via(v: &str) -> &'static str {
    match v {
        "LAN" => "Local network",
        "Relay" => "Through a relay",
        _ => "Internet",
    }
}

/// A member's grey line: "Local network · Seedwell 0.1.6", "Offline", and "until 1 Nov" when its access ends.
/// `device`: the connected device it is (when it is connected); `expires_day`: the member's `expires_s`, written as
/// the host writes a day. This computer's own line only says when its access ends.
#[uniffi::export]
pub fn sync_member_line(member: SyncMember, device: Option<SyncDevice>, expires_day: Option<String>) -> String {
    let until = expires_day.filter(|_| member.expires_s.is_some()).map(|day| format!("until {day}"));
    if member.is_me {
        return until.map(|u| format!("Access {u}")).unwrap_or_default();
    }
    let reach = device.filter(|_| member.online).map(|d| {
        [via(&d.via), d.client.as_str()].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · ")
    });
    [Some(reach.unwrap_or_else(|| "Offline".into())), until].into_iter().flatten().collect::<Vec<_>>().join(" · ")
}

// MARK: Links

/// The core's lowercase reason as a line: a capital first letter, no full stop.
pub(crate) fn sentence(s: &str) -> String {
    let t = trim_spaces(s).trim_end_matches('.');
    let mut c = t.chars();
    c.next().map(|first| first.to_uppercase().chain(c).collect()).unwrap_or_default()
}

pub(crate) fn link_line(text: &str, info: Option<&SyncLinkInfo>, problem: Option<&str>) -> String {
    let t = text.trim();
    if t.is_empty() {
        return String::new();
    }
    let Some(info) = info else {
        if let Some(p) = problem {
            return sentence(p);
        }
        let upper = t.to_uppercase();
        return if ["SWW-", "SWR-", "SWE-"].iter().any(|k| upper.starts_with(k)) {
            "Folders are shared with links now: ask for a seedwell:// link".into()
        } else {
            "Not a Seedwell folder link".into()
        };
    };
    if info.already_synced {
        return concat!("This folder is already synced on this ", computer!()).into();
    }
    if info.expired {
        return "This link has expired: ask for a new one".into();
    }
    let chat = info.channel_name.as_ref().map(|c| {
        format!("with the channel “{c}”{}", if info.channel_joined { " (joined already)" } else { "" })
    });
    [Some(role(info.role).to_string()), info.name.clone(), chat].into_iter().flatten().collect::<Vec<_>>().join(" · ")
}

/// "Read & write · Photos" for a pasted link (and "· with the channel “Family”" when it joins one), or why it cannot
/// be used: the core's `problem` when it gave one ("This link was made by a newer Seedwell…"), an old key, not a
/// link, a folder synced here already, an expired link. Empty while nothing is pasted.
#[uniffi::export]
pub fn sync_link_line(text: String, info: Option<SyncLinkInfo>, problem: Option<String>) -> String {
    link_line(&text, info.as_ref(), problem.as_deref())
}

/// A pasteboard text worth prefilling the add modal with: a `seedwell://sync/` link.
#[uniffi::export]
pub fn sync_looks_like_link(text: String) -> bool {
    let t = text.trim();
    t.chars().count() < 2_000 && t.to_lowercase().starts_with("seedwell://sync/")
}

/// The folder's name a link gives: its name hint, else the first group of its id.
pub(crate) fn suggested_name(info: &SyncLinkInfo) -> String {
    match info.name.as_deref().map(trim_spaces) {
        Some(n) if !n.is_empty() => n.into(),
        _ => info.folder_id.split('-').find(|s| !s.is_empty()).unwrap_or("Folder").into(),
    }
}

#[uniffi::export]
pub fn sync_suggested_name(info: SyncLinkInfo) -> String {
    suggested_name(&info)
}

// MARK: Invitation links and the sharing period

/// Whether an invitation link still admits someone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum SyncInviteState {
    Open,
    Used,
    Expired,
    Revoked,
}

pub(crate) fn invite_state(i: &SyncInvite, now_s: u64) -> SyncInviteState {
    if i.revoked {
        SyncInviteState::Revoked
    } else if i.max_uses > 0 && i.uses >= i.max_uses {
        SyncInviteState::Used
    } else if i.expires_s.is_some_and(|e| e <= now_s) {
        SyncInviteState::Expired
    } else {
        SyncInviteState::Open
    }
}

#[uniffi::export]
pub fn sync_invite_state(invite: SyncInvite, now_unix_s: u64) -> SyncInviteState {
    invite_state(&invite, now_unix_s)
}

/// An invitation link's line in the Share dialog: "Read & write · 0 of 1 used", "· 3 uses, unlimited", or "· Used",
/// "· Expired", "· Revoked".
#[uniffi::export]
pub fn sync_invite_line(invite: SyncInvite, now_unix_s: u64) -> String {
    let i = &invite;
    let state = match invite_state(i, now_unix_s) {
        SyncInviteState::Revoked => "Revoked".into(),
        SyncInviteState::Used => "Used".into(),
        SyncInviteState::Expired => "Expired".into(),
        SyncInviteState::Open if i.max_uses == 0 => format!("{}, unlimited", d::count(u64::from(i.uses), "use")),
        SyncInviteState::Open => format!("{} of {} used", i.uses, i.max_uses),
    };
    format!("{} · {state}", role(i.role))
}

/// The grey line under an open link: "Link until 1 Nov · with the channel “Family”" (empty without either).
/// `expires_day`: the link's `expires_s`, written as the host writes a day.
#[uniffi::export]
pub fn sync_invite_detail(invite: SyncInvite, expires_day: Option<String>) -> String {
    let until = expires_day.filter(|_| invite.expires_s.is_some()).map(|day| format!("Link until {day}"));
    let chat = invite.channel_name.map(|c| format!("with the channel “{c}”"));
    [until, chat].into_iter().flatten().collect::<Vec<_>>().join(" · ")
}

/// The links of the Share dialog: the ones that still admit someone, newest first, at most six; "and 2 more" under
/// them for the rest.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncActiveLinks {
    pub links: Vec<SyncInvite>,
    pub more: Option<String>,
}

#[uniffi::export]
pub fn sync_active_links(invites: Vec<SyncInvite>, now_unix_s: u64) -> SyncActiveLinks {
    let mut open: Vec<SyncInvite> = invites.into_iter().filter(|i| invite_state(i, now_unix_s) == SyncInviteState::Open).collect();
    open.sort_by_key(|i| std::cmp::Reverse(i.created_s));
    let more = open.len().saturating_sub(LINKS_SHOWN);
    open.truncate(LINKS_SHOWN);
    SyncActiveLinks { links: open, more: (more > 0).then(|| format!("and {more} more")) }
}

/// What the Share dialog's next link does, in words: "One Mac can join with this link within 7 days, as Read &
/// write." `uses` and `link_days` 0: no limit. `chat`: the channel the link joins as well.
#[uniffi::export]
pub fn sync_share_summary(role: SyncRole, uses: u32, link_days: u32, chat: Option<String>) -> String {
    let who = match uses {
        1 => concat!("One ", computer!()).to_string(),
        0 => concat!("Any number of ", computer!(), "s").to_string(),
        n => format!(concat!("Up to {} ", computer!(), "s"), n),
    };
    let when = if link_days > 0 { format!(" within {}", d::count(u64::from(link_days), "day")) } else { String::new() };
    let also = chat.map(|c| format!(" It joins the channel “{c}” too.")).unwrap_or_default();
    format!("{who} can join with this link{when}, as {}.{also}", self::role(role))
}

/// The roles a device may hand out with a link: an owner any but Owner (co-owners are made on the folder page), an
/// admin Read & write and Read only. In the order the dialog offers them.
#[uniffi::export]
pub fn sync_share_roles(role: SyncRole) -> Vec<SyncRole> {
    if role == SyncRole::Owner {
        vec![SyncRole::ReadWrite, SyncRole::ReadOnly, SyncRole::Admin]
    } else {
        vec![SyncRole::ReadWrite, SyncRole::ReadOnly]
    }
}

/// One entry of a choice menu: the value and how it reads.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncChoice {
    pub value: u32,
    pub label: String,
}

fn choices(list: &[(u32, &str)]) -> Vec<SyncChoice> {
    list.iter().map(|(value, label)| SyncChoice { value: *value, label: (*label).into() }).collect()
}

/// How often a link may be used when it is not single use (0: unlimited).
#[uniffi::export]
pub fn sync_share_use_choices() -> Vec<SyncChoice> {
    choices(&[(5, "5 times"), (10, "10 times"), (0, "Unlimited")])
}

/// How long a link is valid, and the lengths of a sharing period, in days.
#[uniffi::export]
pub fn sync_share_day_choices() -> Vec<SyncChoice> {
    choices(&[(1, "1 day"), (7, "7 days"), (30, "30 days")])
}

/// The Extend menu of a running sharing period: "+ 1 day", "+ 7 days", "+ 30 days".
#[uniffi::export]
pub fn sync_sharing_extend_choices() -> Vec<SyncChoice> {
    sync_share_day_choices().into_iter().map(|c| SyncChoice { label: format!("+ {}", c.label), ..c }).collect()
}

/// The limits of the next link, 0 = none.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncLinkLimits {
    pub uses: u32,
    pub link_days: u32,
}

/// The next link from the dialog's switches: one use unless "Single use" is off (then `uses`), valid for `link_days`
/// while "Link expires" is on.
#[uniffi::export]
pub fn sync_link_limits(single_use: bool, uses: u32, expires: bool, link_days: u32) -> SyncLinkLimits {
    SyncLinkLimits { uses: if single_use { 1 } else { uses }, link_days: if expires { link_days } else { 0 } }
}

/// The end of a sharing period of `days` started now (Unix seconds); `days` 0: shared with no end (0).
#[uniffi::export]
pub fn sync_sharing_end(days: u32, now_unix_s: u64) -> u64 {
    if days == 0 { 0 } else { now_unix_s + u64::from(days) * DAY_S }
}

/// The end of a sharing period extended by `days` from its current end (or from now, when it ended already). Without
/// a period, one of `days` starts now.
#[uniffi::export]
pub fn sync_sharing_extended(until_s: Option<u64>, days: u32, now_unix_s: u64) -> u64 {
    match until_s {
        Some(until) => until.max(now_unix_s) + u64::from(days) * DAY_S,
        None => sync_sharing_end(days, now_unix_s),
    }
}

/// The time left of a sharing period: "Sharing ends in 6 days, 23 hours", "… in 5 hours, 12 minutes", "… in 12
/// minutes", "Sharing has ended".
#[uniffi::export]
pub fn sync_sharing_countdown(until_s: u64, now_unix_s: u64) -> String {
    if until_s <= now_unix_s {
        return "Sharing has ended".into();
    }
    let left = until_s - now_unix_s;
    let (days, hours, minutes) = (left / DAY_S, left % DAY_S / 3_600, left % 3_600 / 60);
    let parts = if days > 0 {
        [d::count(days, "day"), d::count(hours, "hour")].join(", ")
    } else if hours > 0 {
        [d::count(hours, "hour"), d::count(minutes, "minute")].join(", ")
    } else {
        d::count(minutes.max(1), "minute")
    };
    format!("Sharing ends in {parts}")
}

/// The folder's sharing period under its setting: "When it ends (1 Nov), every Mac but the owners' leaves the
/// folder, and links stop working." `until_day`: the end, written as the host writes a day (`None`: no end). `owner`:
/// this computer sets it.
#[uniffi::export]
pub fn sync_sharing_line(until_day: Option<String>, owner: bool) -> String {
    match until_day {
        Some(day) => format!(concat!("When it ends ({}), every ", computer!(), " but the owners’ leaves the folder, and links stop working."), day),
        None if owner => concat!("Shared until you stop it. ", computer!(), "s you remove keep the files they have.").into(),
        None => "Shared until the owner stops it.".into(),
    }
}

/// The Share dialog of a folder not synced yet (opened from the file manager's Share menu): what Copy Link does
/// first. `path_shown`: the folder as the host shows a path.
#[uniffi::export]
pub fn sync_share_new_note(path_shown: String) -> String {
    format!(concat!("{} is not synced yet. Copy Link starts syncing it from this ", computer!(), ", with you as its owner, and makes the link."), path_shown)
}

// MARK: The add and edit modals

/// The add modal's two ways in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum SyncAddMode {
    /// A folder of this computer, shared from here.
    Local,
    /// A folder someone shared: a pasted link.
    Shared,
}

/// The modes in the order the modal offers them.
#[uniffi::export]
pub fn sync_add_modes() -> Vec<SyncAddMode> {
    vec![SyncAddMode::Local, SyncAddMode::Shared]
}

/// "From this Mac", "Shared with me".
#[uniffi::export]
pub fn sync_add_mode_label(mode: SyncAddMode) -> String {
    match mode {
        SyncAddMode::Local => concat!("From this ", computer!()),
        SyncAddMode::Shared => "Shared with me",
    }
    .into()
}

/// The add modal's choices (SPEC 7G.5). The host keeps it as a value, edits the plain fields itself and asks the
/// `sync_add_*` functions for the rest.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncAddForm {
    pub mode: SyncAddMode,
    /// From this computer: the folder chosen.
    pub folder: String,
    /// Shared with me: what was pasted, what it is, why it is not usable (the core's words; none when it is a link
    /// or nothing is pasted), and where it goes.
    pub key: String,
    pub info: Option<SyncLinkInfo>,
    pub problem: Option<String>,
    pub location: String,
    /// The user picked the location: a new link does not move it any more.
    pub location_chosen: bool,
}

/// A fresh form: From this computer, nothing chosen.
#[uniffi::export]
pub fn sync_add_form_new() -> SyncAddForm {
    SyncAddForm {
        mode: SyncAddMode::Local,
        folder: String::new(),
        key: String::new(),
        info: None,
        problem: None,
        location: String::new(),
        location_chosen: false,
    }
}

/// The line under the link field (`sync_link_line` of what was pasted).
#[uniffi::export]
pub fn sync_add_key_line(form: SyncAddForm) -> String {
    link_line(&form.key, form.info.as_ref(), form.problem.as_deref())
}

pub(crate) fn key_usable(form: &SyncAddForm) -> bool {
    form.info.as_ref().is_some_and(|i| !i.expired && !i.already_synced)
}

/// The pasted text is a link that can be joined.
#[uniffi::export]
pub fn sync_add_key_usable(form: SyncAddForm) -> bool {
    key_usable(&form)
}

/// Start may be pressed: a folder is chosen, or a usable link is pasted and has a location.
#[uniffi::export]
pub fn sync_add_can_start(form: SyncAddForm) -> bool {
    match form.mode {
        SyncAddMode::Local => !trim_spaces(&form.folder).is_empty(),
        SyncAddMode::Shared => key_usable(&form) && !trim_spaces(&form.location).is_empty(),
    }
}

/// A new link was pasted (`info`: what the core read in it; `problem`: why not, when it read nothing): the location
/// follows it (`default_location`) unless the user chose one.
#[uniffi::export]
pub fn sync_add_set_key(
    form: SyncAddForm,
    text: String,
    info: Option<SyncLinkInfo>,
    default_location: Option<String>,
    problem: Option<String>,
) -> SyncAddForm {
    let location = if form.location_chosen {
        form.location
    } else if info.is_none() {
        String::new()
    } else {
        default_location.unwrap_or_default()
    };
    let problem = if info.is_none() { problem } else { None };
    SyncAddForm { key: text, info, problem, location, ..form }
}

/// The user chose where the folder goes: `parent`; the folder itself is named after the link.
#[uniffi::export]
pub fn sync_add_choose_parent(form: SyncAddForm, parent: String) -> SyncAddForm {
    let name = form.info.as_ref().map_or_else(|| "Folder".into(), suggested_name);
    SyncAddForm { location: Path::new(&parent).join(name).display().to_string(), location_chosen: true, ..form }
}

/// The path with links resolved, as the file manager names it; a path that is not there stays as it is.
fn real_path(p: &str) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| PathBuf::from(p))
}

/// The folder `name` next to `next` (the new place the core picked for a joined folder) when that name was taken by
/// a folder that exists and is not synced here, so `next` got a number: the one to offer for joining into. `synced`:
/// the paths of the folders synced here.
#[uniffi::export]
pub fn sync_existing_folder(name: String, next: String, synced: Vec<String>) -> Option<String> {
    let plain = Path::new(&next).parent().unwrap_or(Path::new("")).join(&name).display().to_string();
    if plain == next || !Path::new(&plain).is_dir() {
        return None;
    }
    let real = real_path(&plain);
    (!synced.iter().any(|s| real_path(s) == real)).then_some(plain)
}

/// Under the join form's Location: syncing into a folder that exists keeps its files and shares them; else, when a
/// folder of that name is there already (`existing`), that it can be chosen instead of a new one. `existing_shown`:
/// that folder as the host shows a path.
#[uniffi::export]
pub fn sync_location_note(location: String, existing: Option<String>, existing_shown: String) -> Option<String> {
    let existing = existing?;
    Some(if location == existing {
        format!("{existing_shown} exists already. The files in it stay and become part of the shared folder; where they differ from the others’, both versions are kept.")
    } else {
        format!("{existing_shown} exists already. Choose it in Location to sync into it instead of a new folder.")
    })
}

/// Why a folder name cannot be used (the edit modal's rename), or `None` when it can.
#[uniffi::export]
pub fn sync_folder_name_problem(name: String) -> Option<String> {
    let n = trim_spaces(&name);
    if n.is_empty() {
        Some("Give the folder a name.".into())
    } else if n.contains('/') || n.contains(':') {
        Some("A folder name cannot contain “/” or “:”.".into())
    } else if n.starts_with('.') {
        Some("A folder name cannot start with a dot.".into())
    } else if trs_platform::fs::name_problem(n).is_some() {
        // What else this system refuses (Windows: more characters, a trailing dot or space, device names).
        Some("A folder name cannot contain \\ * ? \" < > |, end with a dot or a space, or be a device name such as CON.".into())
    } else {
        None
    }
}

/// The edit modal's Share section: what this computer's role lets it do.
#[uniffi::export]
pub fn sync_edit_share_note(role: SyncRole) -> String {
    match role {
        SyncRole::Owner => "You own this folder. Links you make admit devices at the role you pick; roles are changed on the folder page.",
        SyncRole::Admin => "You are an admin: you can make links for others to join.",
        SyncRole::ReadWrite | SyncRole::ReadOnly => "Only the folder’s owner and admins can share it.",
    }
    .into()
}

// MARK: Removing, errors and alerts

/// The question of removing a folder (the menu's confirmation, the folder page).
#[uniffi::export]
pub fn sync_remove_question(name: String) -> String {
    format!(concat!("Stop syncing “{}” on this ", computer!(), "? Other devices keep their copy."), name)
}

/// The option of that question.
#[uniffi::export]
pub fn sync_remove_trash_label() -> String {
    concat!("Also move the files to the ", trash!()).into()
}

/// The hover text of Remove under that question, with the option (`trash`) or without.
#[uniffi::export]
pub fn sync_remove_help(trash: bool) -> String {
    if trash { concat!("Stop syncing and move the folder to the ", trash!()) } else { "Stop syncing; the files stay" }.into()
}

/// The plain sentence of a core error ("This folder is no longer synced on this Mac.").
#[uniffi::export]
pub fn core_error_message(error: CoreError) -> String {
    match error {
        CoreError::Failed { message } | CoreError::InvalidSource { message } => message,
        CoreError::Invalid { problems } => problems.join("\n"),
        CoreError::NotFound { .. } => concat!("This folder is no longer synced on this ", computer!(), ".").into(),
    }
}

/// An alert: its title and its text.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncAlert {
    pub title: String,
    pub text: String,
}

/// The title of the button that closes an alert. (The Mac's NSAlert gives its own; a host that draws the alert itself
/// uses this.)
#[uniffi::export]
pub fn alert_ok_title() -> String {
    "OK".into()
}

/// The synced folder a path is in (the innermost one), for the file manager's Share menu.
#[uniffi::export]
pub fn sync_folder_containing(folders: Vec<SyncFolder>, path: String) -> Option<SyncFolder> {
    let real = real_path(&path);
    folders.into_iter().filter(|f| real.starts_with(real_path(&f.path))).fold(None, |best: Option<SyncFolder>, f| match best {
        Some(b) if b.path.chars().count() >= f.path.chars().count() => Some(b),
        _ => Some(f),
    })
}

/// Why this computer may not share `folder` (`None`: it may).
#[uniffi::export]
pub fn sync_share_refusal(folder: SyncFolder) -> Option<SyncAlert> {
    (!sync_can_share(folder.role)).then(|| SyncAlert {
        title: format!("Only the owner and admins can share “{}”", folder.name),
        text: format!(concat!("This ", computer!(), " is {} in it. Ask the owner or an admin for a link."), role(folder.role)),
    })
}

/// Stopping a folder failed (`error`: the core's sentence).
#[uniffi::export]
pub fn sync_remove_failed(name: String, error: String) -> SyncAlert {
    SyncAlert { title: format!("Couldn't stop syncing “{name}”"), text: error }
}

/// The folder stopped syncing but could not go to the Trash (`Core::sync_remove` returns it). `path_shown`: where it is,
/// as user-visible text writes a path.
pub(crate) fn sync_trash_failed(name: String, path_shown: String) -> SyncAlert {
    SyncAlert {
        title: format!(concat!("Couldn't move “{}” to the ", trash!()), name),
        text: format!(concat!("It is no longer synced on this ", computer!(), ". Its files are still in {}."), path_shown),
    }
}

/// Why the folder `name` cannot go to the Trash, and what to do instead: the text of [`sync_remove_failed`]. Checked
/// before the folder stops syncing, so it is still synced.
pub(crate) fn sync_trash_refusal(name: &str, e: &TrashError) -> String {
    let instead = format!("Turn off “{}” to stop syncing it", sync_remove_trash_label());
    match e {
        TrashError::OtherVolume { .. } => format!(
            concat!("“{}” is on another volume than the ", trash!(), ", so it can't be moved there. {}, then delete the folder in ", file_manager!(), "."),
            name, instead
        ),
        TrashError::WouldDelete { reason } => format!(
            concat!("The ", trash!(), " would delete “{}” at once instead of keeping it ({}). {}, then delete the folder in ", file_manager!(), "."),
            name, reason, instead
        ),
        TrashError::NoTrash => format!(concat!("There is no ", trash!(), " to move “{}” to. {}."), name, instead),
        TrashError::Io(err) => format!(concat!("“{}” can't be moved to the ", trash!(), " ({}). {}."), name, err, instead),
    }
}

/// Resolving the conflict of `original` failed.
#[uniffi::export]
pub fn sync_resolve_failed(original: String, error: String) -> SyncAlert {
    SyncAlert { title: format!("Couldn't resolve the conflict in “{original}”"), text: error }
}

/// Giving the device `device` another role failed.
#[uniffi::export]
pub fn sync_set_role_failed(device: String, error: String) -> SyncAlert {
    SyncAlert { title: format!("Couldn't change {device}’s role"), text: error }
}

/// Removing the device `device` from the folder or channel `name` failed.
#[uniffi::export]
pub fn sync_remove_device_failed(device: String, name: String, error: String) -> SyncAlert {
    SyncAlert { title: format!("Couldn't remove {device} from “{name}”"), text: error }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A folder of 1 GB, up to date, owned here (tests of this crate build on it).
    pub(crate) fn folder(name: &str, state: SyncState) -> SyncFolder {
        SyncFolder {
            id: format!("ID-{name}"),
            name: name.into(),
            path: format!("/Users/me/Seedwell/{name}"),
            access: SyncAccess::ReadWrite,
            role: SyncRole::Owner,
            share_until_s: None,
            state,
            files: 10,
            bytes: 1_000_000_000,
            need_files: 0,
            need_bytes: 0,
            rate_in: 0,
            rate_out: 0,
            connected: 0,
            members: 0,
            conflicts: 0,
            lan_only: false,
            issues: Vec::new(),
            error: None,
            last_synced_s: None,
            channel_id: None,
        }
    }

    fn with(state: SyncState, f: impl FnOnce(&mut SyncFolder)) -> SyncFolder {
        let mut x = folder("Photos", state);
        f(&mut x);
        x
    }

    fn syncing(need: u64, rate: u64) -> SyncFolder {
        with(SyncState::Syncing, |f| {
            f.need_bytes = need;
            f.need_files = if need > 0 { 3 } else { 0 };
            f.rate_in = rate;
        })
    }

    #[test]
    fn row_subtitles_follow_the_state() {
        let up = |connected: u32, conflicts: u32| {
            subtitle(&with(SyncState::UpToDate, |f| {
                f.connected = connected;
                f.conflicts = conflicts;
            }))
        };
        assert_eq!(up(3, 0), "Up to date · 3 devices");
        assert_eq!(up(1, 0), "Up to date · 1 device");
        assert_eq!(up(0, 0), "Up to date");
        assert_eq!(up(2, 1), "1 conflict · 2 devices");
        assert_eq!(up(0, 2), "2 conflicts");
        assert_eq!(subtitle(&syncing(580_000_000, 12_000_000)), "Syncing · 42% · 12 MB/s");
        assert_eq!(subtitle(&with(SyncState::Syncing, |f| f.bytes = 0)), "Syncing", "nothing counted yet");
        assert_eq!(subtitle(&with(SyncState::Syncing, |f| { f.bytes = 0; f.need_files = 3 })), "Syncing · 3 files to go");
        assert_eq!(subtitle(&syncing(1, 0)), "Syncing · 99%", "never 100 % while syncing");
        assert_eq!(subtitle(&with(SyncState::Syncing, |f| f.conflicts = 1)), "1 conflict · Syncing · 99%");
        assert_eq!(subtitle(&folder("A", SyncState::WaitingForDevices)), "Waiting for devices");
        assert_eq!(subtitle(&with(SyncState::Scanning, |f| f.conflicts = 1)), "1 conflict · Scanning…");
        assert_eq!(subtitle(&folder("A", SyncState::WaitingForAdmin)), "Waiting for the owner or an admin");
        assert_eq!(subtitle(&folder("A", SyncState::Missing)), "Folder missing");
        assert_eq!(subtitle(&with(SyncState::Paused, |f| f.connected = 2)), "Paused");
        assert_eq!(subtitle(&folder("A", SyncState::Starting)), "Starting…");
        assert_eq!(subtitle(&folder("A", SyncState::Error)), "Can't sync");
        assert_eq!(subtitle(&with(SyncState::Error, |f| f.error = Some("disk full".into()))), "Can't sync · disk full");
    }

    #[test]
    fn the_badge_says_what_needs_the_user_first() {
        assert_eq!(badge(&with(SyncState::UpToDate, |f| f.conflicts = 1)), SyncBadge::Attention);
        assert_eq!(badge(&syncing(500_000_000, 0)), SyncBadge::Progress { fraction: 0.5 });
        assert_eq!(badge(&with(SyncState::WaitingForDevices, |f| f.bytes = 0)), SyncBadge::Waiting);
        assert_eq!(badge(&with(SyncState::WaitingForDevices, |f| f.need_bytes = 0)), SyncBadge::Waiting, "has it all: still waiting, not a full ring");
        assert_eq!(badge(&with(SyncState::WaitingForDevices, |f| f.need_bytes = f.bytes / 2)), SyncBadge::Waiting, "half here: waiting, not a ring");
        assert_eq!(badge(&folder("A", SyncState::WaitingForAdmin)), SyncBadge::Waiting);
        assert_eq!(sync_badge_label(SyncBadge::Waiting), "waiting");
        assert_eq!(badge(&folder("A", SyncState::Scanning)), SyncBadge::Progress { fraction: 0.0 });
        assert_eq!(badge(&folder("A", SyncState::Missing)), SyncBadge::Missing);
        assert_eq!(badge(&with(SyncState::Missing, |f| f.conflicts = 1)), SyncBadge::Missing, "missing wins over conflicts");
        assert_eq!(badge(&folder("A", SyncState::Paused)), SyncBadge::Paused);
        assert_eq!(badge(&folder("A", SyncState::Error)), SyncBadge::Attention);
        assert_eq!(badge(&folder("A", SyncState::UpToDate)), SyncBadge::UpToDate);
        assert_eq!(sync_badge_label(SyncBadge::Progress { fraction: 0.3 }), "syncing");
        assert_eq!(sync_badge_label(SyncBadge::Attention), "needs attention");
    }

    #[test]
    fn the_file_manager_badge_shows_what_moves_and_a_conflict_when_nothing_does() {
        use SyncFileBadge as B;
        let b = |state, f: &dyn Fn(&mut SyncFolder)| {
            let mut x = folder("A", state);
            f(&mut x);
            sync_file_badge(x)
        };
        assert_eq!(b(SyncState::UpToDate, &|f| f.connected = 2), B::Synced);
        assert_eq!(b(SyncState::UpToDate, &|_| {}), B::Offline, "no device online");
        assert_eq!(b(SyncState::UpToDate, &|f| f.rate_out = 5), B::Uploading);
        assert_eq!(b(SyncState::Syncing, &|f| f.rate_in = 5), B::Downloading);
        assert_eq!(b(SyncState::Syncing, &|f| f.need_files = 2), B::Downloading);
        assert_eq!(b(SyncState::Syncing, &|f| { f.rate_in = 5; f.rate_out = 5 }), B::Syncing);
        assert_eq!(b(SyncState::Syncing, &|f| f.rate_out = 5), B::Uploading);
        assert_eq!(b(SyncState::Syncing, &|_| {}), B::Syncing);
        assert_eq!(b(SyncState::Scanning, &|_| {}), B::Syncing);
        assert_eq!(b(SyncState::WaitingForAdmin, &|_| {}), B::Offline);
        assert_eq!(b(SyncState::Paused, &|f| f.conflicts = 1), B::Paused);
        assert_eq!(b(SyncState::Missing, &|_| {}), B::Error);
        assert_eq!(b(SyncState::Error, &|_| {}), B::Error);
        assert_eq!(b(SyncState::UpToDate, &|f| { f.connected = 1; f.conflicts = 1 }), B::Conflict);
        assert_eq!(b(SyncState::WaitingForDevices, &|f| f.conflicts = 1), B::Conflict);
        assert_eq!(b(SyncState::Syncing, &|f| { f.rate_in = 5; f.conflicts = 1 }), B::Downloading, "moving wins over a conflict");
    }

    fn link(role: SyncRole, name: Option<&str>) -> SyncLinkInfo {
        SyncLinkInfo {
            role,
            name: name.map(Into::into),
            folder_id: "MFZWI3D-BONSGYC".into(),
            already_synced: false,
            expires_s: None,
            expired: false,
            channel_name: None,
            channel_joined: false,
        }
    }

    #[test]
    fn pasted_links_are_shown_for_what_they_are() {
        let line = |text: &str, info: Option<SyncLinkInfo>| link_line(text, info.as_ref(), None);
        assert_eq!(line("seedwell://sync/ABC", Some(link(SyncRole::ReadWrite, Some("Photos")))), "Read & write · Photos");
        let chat = SyncLinkInfo { channel_name: Some("Family".into()), ..link(SyncRole::ReadWrite, Some("Photos")) };
        assert_eq!(line("seedwell://sync/ABC", Some(chat)), "Read & write · Photos · with the channel “Family”", "one link joins the folder and its chat");
        let joined = SyncLinkInfo { channel_name: Some("Family".into()), channel_joined: true, ..link(SyncRole::ReadOnly, None) };
        assert_eq!(line("seedwell://sync/ABC", Some(joined)), "Read only · with the channel “Family” (joined already)");
        assert_eq!(line("seedwell://sync/ABC", Some(link(SyncRole::ReadOnly, None))), "Read only");
        assert_eq!(line("hello", None), "Not a Seedwell folder link");
        assert_eq!(line("SWW-ABCDEF", None), "Folders are shared with links now: ask for a seedwell:// link");
        assert_eq!(line("swr-abcdef", None), "Folders are shared with links now: ask for a seedwell:// link");
        assert_eq!(line("  ", None), "");
        assert_eq!(
            sync_link_line("seedwell://sync/9ABC".into(), None, Some("this link was made by a newer Seedwell: update Seedwell to use it.".into())),
            "This link was made by a newer Seedwell: update Seedwell to use it",
            "the core's reason, as a line"
        );
        let mine = SyncLinkInfo { already_synced: true, ..link(SyncRole::ReadWrite, None) };
        assert_eq!(line("x", Some(mine)), concat!("This folder is already synced on this ", computer!()));
        let old = SyncLinkInfo { expired: true, ..link(SyncRole::ReadWrite, None) };
        assert_eq!(line("x", Some(old)), "This link has expired: ask for a new one");
        assert!(sync_looks_like_link(" seedwell://sync/ABC?name=x\n".into()));
        assert!(sync_looks_like_link("SEEDWELL://SYNC/ABC".into()));
        assert!(!sync_looks_like_link("SWR-abc".into()), "bare keys are not links");
        assert!(!sync_looks_like_link("https://example.com".into()));
        assert!(!sync_looks_like_link("magnet:?xt=urn:btih:abc".into()));
        assert!(!sync_looks_like_link(format!("seedwell://sync/{}", "A".repeat(2_000))), "a whole document is no link");
    }

    #[test]
    fn a_reason_reads_as_a_line() {
        assert_eq!(sentence("  disk full.. "), "Disk full");
        assert_eq!(sentence("éclair"), "Éclair");
        assert_eq!(sentence(""), "");
        assert_eq!(sentence("..."), "");
    }

    #[test]
    fn a_link_names_its_folder_by_its_hint_or_its_id() {
        assert_eq!(suggested_name(&link(SyncRole::ReadOnly, None)), "MFZWI3D");
        assert_eq!(suggested_name(&link(SyncRole::ReadOnly, Some("  Photos "))), "Photos");
        assert_eq!(suggested_name(&link(SyncRole::ReadOnly, Some("  "))), "MFZWI3D");
        assert_eq!(suggested_name(&SyncLinkInfo { folder_id: "-X-Y".into(), ..link(SyncRole::ReadOnly, None) }), "X");
        assert_eq!(suggested_name(&SyncLinkInfo { folder_id: String::new(), ..link(SyncRole::ReadOnly, None) }), "Folder");
    }

    const NOW: u64 = 1_800_000_000;

    fn invite(uses: u32, max: u32, expires: Option<u64>, revoked: bool, chat: Option<&str>) -> SyncInvite {
        SyncInvite {
            id: format!("I{uses}{max}"),
            link: "seedwell://sync/I".into(),
            role: SyncRole::ReadWrite,
            max_uses: max,
            uses,
            expires_s: expires,
            created_s: NOW - 60,
            revoked,
            channel_id: chat.map(|c| format!("C-{c}")),
            channel_name: chat.map(Into::into),
        }
    }

    #[test]
    fn invitation_links_say_what_is_left() {
        let line = |i: SyncInvite| sync_invite_line(i, NOW);
        assert_eq!(invite_state(&invite(0, 1, None, false, None), NOW), SyncInviteState::Open);
        assert_eq!(line(invite(0, 1, None, false, None)), "Read & write · 0 of 1 used");
        assert_eq!(invite_state(&invite(1, 1, None, false, None), NOW), SyncInviteState::Used, "a single-use link is spent once used");
        assert_eq!(line(invite(1, 1, None, false, None)), "Read & write · Used");
        assert_eq!(line(invite(3, 0, None, false, None)), "Read & write · 3 uses, unlimited");
        assert_eq!(line(invite(1, 0, None, false, None)), "Read & write · 1 use, unlimited");
        assert_eq!(sync_invite_state(invite(0, 1, Some(NOW), false, None), NOW), SyncInviteState::Expired);
        assert_eq!(line(invite(0, 1, Some(NOW), false, None)), "Read & write · Expired");
        assert_eq!(invite_state(&invite(0, 1, Some(NOW + 1), false, None), NOW), SyncInviteState::Open);
        assert_eq!(line(invite(1, 1, Some(NOW), true, None)), "Read & write · Revoked", "revoked says so first");
        assert_eq!(sync_invite_detail(invite(0, 5, Some(NOW + DAY_S), false, None), Some("1 Nov".into())), "Link until 1 Nov");
        assert_eq!(sync_invite_detail(invite(0, 1, None, false, None), None), "", "no dates, no line");
        assert_eq!(sync_invite_detail(invite(0, 1, None, false, None), Some("1 Nov".into())), "", "a day without an end is not shown");
        assert_eq!(sync_invite_detail(invite(0, 1, None, false, Some("Family")), None), "with the channel “Family”");
        assert_eq!(
            sync_invite_detail(invite(0, 1, Some(NOW + 5), false, Some("Family")), Some("2 Nov".into())),
            "Link until 2 Nov · with the channel “Family”"
        );
    }

    #[test]
    fn the_share_dialog_lists_open_links_newest_first_and_counts_the_rest() {
        let mut list: Vec<SyncInvite> = (0..9)
            .map(|n| SyncInvite { id: format!("L{n}"), created_s: NOW - 100 + n, ..invite(0, 1, None, false, None) })
            .collect();
        list.push(SyncInvite { id: "used".into(), created_s: NOW, ..invite(1, 1, None, false, None) });
        list.push(SyncInvite { id: "revoked".into(), created_s: NOW, ..invite(0, 1, None, true, None) });
        let a = sync_active_links(list, NOW);
        assert_eq!(a.links.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), ["L8", "L7", "L6", "L5", "L4", "L3"]);
        assert_eq!(a.more.as_deref(), Some("and 3 more"));
        let few = sync_active_links(vec![invite(0, 1, None, false, None)], NOW);
        assert_eq!((few.links.len(), few.more), (1, None));
    }

    #[test]
    fn the_share_dialog_says_what_the_next_link_does() {
        let c = computer!();
        assert_eq!(
            sync_share_summary(SyncRole::ReadWrite, 1, 7, None),
            format!("One {c} can join with this link within 7 days, as Read & write.")
        );
        assert_eq!(sync_share_summary(SyncRole::ReadOnly, 0, 0, None), format!("Any number of {c}s can join with this link, as Read only."));
        assert_eq!(sync_share_summary(SyncRole::Admin, 5, 1, None), format!("Up to 5 {c}s can join with this link within 1 day, as Admin."));
        assert_eq!(
            sync_share_summary(SyncRole::ReadWrite, 1, 7, Some("Photos".into())),
            format!("One {c} can join with this link within 7 days, as Read & write. It joins the channel “Photos” too.")
        );
        assert_eq!(sync_link_limits(true, 5, true, 7), SyncLinkLimits { uses: 1, link_days: 7 });
        assert_eq!(sync_link_limits(false, 10, false, 7), SyncLinkLimits { uses: 10, link_days: 0 });
        assert_eq!(sync_share_roles(SyncRole::Owner), [SyncRole::ReadWrite, SyncRole::ReadOnly, SyncRole::Admin]);
        assert_eq!(sync_share_roles(SyncRole::Admin), [SyncRole::ReadWrite, SyncRole::ReadOnly]);
        let labels = |v: Vec<SyncChoice>| v.into_iter().map(|c| (c.value, c.label)).collect::<Vec<_>>();
        assert_eq!(labels(sync_share_use_choices()), [(5, "5 times".into()), (10, "10 times".into()), (0, "Unlimited".into())]);
        assert_eq!(labels(sync_share_day_choices()), [(1, "1 day".into()), (7, "7 days".into()), (30, "30 days".into())]);
        assert_eq!(labels(sync_sharing_extend_choices())[0], (1, "+ 1 day".into()));
    }

    #[test]
    fn the_sharing_period_counts_down_and_extends_from_its_end() {
        let t = NOW;
        assert_eq!(sync_sharing_countdown(t + 6 * DAY_S + 23 * 3_600 + 59 * 60, t), "Sharing ends in 6 days, 23 hours");
        assert_eq!(sync_sharing_countdown(t + 2 * DAY_S + 30, t), "Sharing ends in 2 days, 0 hours");
        assert_eq!(sync_sharing_countdown(t + 5 * 3_600 + 12 * 60, t), "Sharing ends in 5 hours, 12 minutes");
        assert_eq!(sync_sharing_countdown(t + 30, t), "Sharing ends in 1 minute");
        assert_eq!(sync_sharing_countdown(t + 12 * 60, t), "Sharing ends in 12 minutes");
        assert_eq!(sync_sharing_countdown(t, t), "Sharing has ended");
        assert_eq!(sync_sharing_countdown(t - 5, t), "Sharing has ended");
        assert_eq!(sync_sharing_line(None, false), "Shared until the owner stops it.");
        assert_eq!(sync_sharing_line(None, true), format!("Shared until you stop it. {}s you remove keep the files they have.", computer!()));
        assert_eq!(
            sync_sharing_line(Some("1 Nov".into()), true),
            format!("When it ends (1 Nov), every {} but the owners’ leaves the folder, and links stop working.", computer!())
        );
        assert_eq!(sync_sharing_end(7, t), t + 7 * DAY_S);
        assert_eq!(sync_sharing_end(0, t), 0, "no end");
        assert_eq!(sync_sharing_extended(Some(t + DAY_S), 7, t), t + 8 * DAY_S, "from the current end");
        assert_eq!(sync_sharing_extended(Some(t - DAY_S), 1, t), t + DAY_S, "an end in the past extends from now");
        assert_eq!(sync_sharing_extended(None, 30, t), t + 30 * DAY_S);
    }

    fn member(name: &str, role: SyncRole, me: bool) -> SyncMember {
        SyncMember { device_id: name.into(), name: name.into(), user: String::new(), role, online: true, is_me: me, expires_s: None }
    }

    #[test]
    fn roles_decide_who_shares_and_members_are_listed_mine_first_then_by_role() {
        assert!(sync_can_share(SyncRole::Owner) && sync_can_share(SyncRole::Admin));
        assert!(!sync_can_share(SyncRole::ReadWrite) && !sync_can_share(SyncRole::ReadOnly));
        let ordered = sync_members_ordered(vec![
            member("Zed", SyncRole::ReadOnly, false),
            member("Bo", SyncRole::Admin, false),
            member("Me", SyncRole::ReadWrite, true),
            member("Al", SyncRole::Owner, false),
            member("ann", SyncRole::Admin, false),
            member("Mac 10", SyncRole::ReadOnly, false),
            member("Mac 9", SyncRole::ReadOnly, false),
        ]);
        let names: Vec<_> = ordered.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["Me", "Al", "ann", "Bo", "Mac 9", "Mac 10", "Zed"], "case aside, numbers by value");
        assert_eq!(sync_role_label(SyncRole::ReadWrite), "Read & write");
        assert_eq!(sync_role_note(SyncRole::ReadOnly), "Receives changes, never sends its own");
        assert_eq!(sync_access_label(SyncAccess::Encrypted), "Encrypted");
    }

    #[test]
    fn a_members_line_says_how_it_is_reached_and_until_when() {
        let me = member("Me", SyncRole::ReadWrite, true);
        assert_eq!(sync_member_line(me.clone(), None, None), "", "this computer's role is on the right already");
        let mine_until = SyncMember { expires_s: Some(NOW), ..me };
        assert_eq!(sync_member_line(mine_until, None, Some("1 Nov".into())), "Access until 1 Nov");
        let off = SyncMember { online: false, ..member("X", SyncRole::ReadOnly, false) };
        assert_eq!(sync_member_line(off.clone(), None, None), "Offline");
        let guest = SyncMember { expires_s: Some(NOW), ..off };
        assert_eq!(sync_member_line(guest, None, Some("1 Nov".into())), "Offline · until 1 Nov", "time-limited access shows its end");
        let dev = |via: &str, client: &str| SyncDevice {
            id: "X".into(),
            name: "X".into(),
            client: client.into(),
            address: String::new(),
            via: via.into(),
            folders: Vec::new(),
            rx_bytes: 0,
            tx_bytes: 0,
        };
        let on = member("X", SyncRole::ReadOnly, false);
        assert_eq!(sync_member_line(on.clone(), Some(dev("LAN", "Seedwell 0.1.6")), None), "Local network · Seedwell 0.1.6");
        assert_eq!(sync_member_line(on.clone(), Some(dev("Relay", "")), None), "Through a relay");
        assert_eq!(sync_member_line(on.clone(), Some(dev("WAN", "Seedwell 0.1.7")), None), "Internet · Seedwell 0.1.7");
        assert_eq!(sync_member_line(on, None, None), "Offline", "online but not connected to this computer");
    }

    #[test]
    fn the_add_modal_starts_only_with_what_it_needs() {
        let mut f = sync_add_form_new();
        assert!(f.mode == SyncAddMode::Local && !sync_add_can_start(f.clone()));
        f.folder = "/Users/me/Novel".into();
        assert!(sync_add_can_start(f.clone()));
        f.mode = SyncAddMode::Shared;
        assert!(!sync_add_can_start(f.clone()));
        f = sync_add_set_key(f, "nonsense".into(), None, None, None);
        assert!(!sync_add_can_start(f.clone()) && f.location.is_empty());
        assert_eq!(sync_add_key_line(f.clone()), "Not a Seedwell folder link");
        let photos = SyncLinkInfo { folder_id: "F".into(), ..link(SyncRole::ReadWrite, Some("Photos")) };
        f = sync_add_set_key(f, "seedwell://sync/x".into(), Some(photos), Some("/Users/me/Seedwell/Photos".into()), Some("ignored".into()));
        assert!(sync_add_can_start(f.clone()) && sync_add_key_usable(f.clone()));
        assert_eq!((f.location.as_str(), f.problem.as_deref()), ("/Users/me/Seedwell/Photos", None), "a problem only without a link");
        // A location the user chose stays when the link changes.
        f = sync_add_choose_parent(f, "/Volumes/Disk".into());
        assert!(f.location == Path::new("/Volumes/Disk").join("Photos").display().to_string() && f.location_chosen);
        let other = SyncLinkInfo { folder_id: "G".into(), ..link(SyncRole::ReadOnly, Some("Other")) };
        f = sync_add_set_key(f, "seedwell://sync/y".into(), Some(other), Some("/Users/me/Seedwell/Other".into()), None);
        assert_eq!(f.location, Path::new("/Volumes/Disk").join("Photos").display().to_string());
        let mine = SyncLinkInfo { already_synced: true, ..link(SyncRole::ReadWrite, None) };
        f = sync_add_set_key(f, "seedwell://sync/z".into(), Some(mine), None, None);
        assert!(!sync_add_can_start(f.clone()), "already synced");
        let old = SyncLinkInfo { expired: true, ..link(SyncRole::ReadWrite, None) };
        f = sync_add_set_key(f, "seedwell://sync/z".into(), Some(old), None, None);
        assert!(!sync_add_can_start(f.clone()), "an expired link admits nobody");
        let bad = sync_add_set_key(sync_add_form_new(), "seedwell://sync/9".into(), None, None, Some("this link was made by a newer Seedwell.".into()));
        assert_eq!(sync_add_key_line(bad), "This link was made by a newer Seedwell");
        // Without a link, the folder is named "Folder".
        assert!(sync_add_choose_parent(sync_add_form_new(), "/x".into()).location.ends_with("Folder"));
        assert_eq!(sync_add_modes(), [SyncAddMode::Local, SyncAddMode::Shared]);
        assert_eq!(sync_add_mode_label(SyncAddMode::Local), format!("From this {}", computer!()));
        assert_eq!(sync_add_mode_label(SyncAddMode::Shared), "Shared with me");
    }

    #[test]
    fn folder_names_are_checked_before_a_rename() {
        assert!(sync_folder_name_problem(" ".into()).is_some());
        assert!(sync_folder_name_problem("a/b".into()).is_some());
        assert!(sync_folder_name_problem("a:b".into()).is_some());
        assert_eq!(sync_folder_name_problem(".x".into()).as_deref(), Some("A folder name cannot start with a dot."));
        assert_eq!(sync_folder_name_problem("Photos 2026".into()), None);
        assert_eq!(sync_folder_name_problem("  Photos ".into()), None);
        // What only Windows refuses.
        for n in ["why?", "a*b", "CON", "trailing."] {
            assert_eq!(sync_folder_name_problem(n.into()).is_some(), cfg!(windows), "{n}");
        }
        assert!(sync_edit_share_note(SyncRole::ReadOnly).starts_with("Only the folder’s owner"));
        assert!(sync_edit_share_note(SyncRole::Owner).starts_with("You own this folder."));
    }

    #[test]
    fn joining_offers_a_folder_of_that_name_that_is_there_already() {
        let root = tempfile::tempdir().unwrap();
        let base = root.path().join("Seedwell");
        std::fs::create_dir_all(base.join("Team")).unwrap();
        let s = |p: PathBuf| p.display().to_string();
        let (team, next) = (s(base.join("Team")), s(base.join("Team 2")));
        assert_eq!(sync_existing_folder("Team".into(), next.clone(), Vec::new()), Some(team.clone()));
        assert_eq!(sync_existing_folder("Team".into(), next.clone(), vec![team.clone()]), None, "a folder synced already is not offered");
        assert_eq!(sync_existing_folder("Team".into(), team.clone(), Vec::new()), None, "the name was free: nothing to offer");
        assert_eq!(sync_existing_folder("Other".into(), s(base.join("Other 2")), Vec::new()), None, "no such folder");
        std::fs::write(base.join("File"), b"x").unwrap();
        assert_eq!(sync_existing_folder("File".into(), s(base.join("File 2")), Vec::new()), None, "a file is no folder");
        let same = |l: &str, e: Option<&str>| sync_location_note(l.into(), e.map(Into::into), e.unwrap_or_default().replace("/Users/me", "~"));
        assert_eq!(same("/Users/me/Seedwell/Team 2", None), None);
        assert_eq!(
            same("/Users/me/Seedwell/Team 2", Some("/Users/me/Seedwell/Team")).as_deref(),
            Some("~/Seedwell/Team exists already. Choose it in Location to sync into it instead of a new folder.")
        );
        assert!(same("/Users/me/Seedwell/Team", Some("/Users/me/Seedwell/Team")).unwrap().starts_with("~/Seedwell/Team exists already. The files in it stay"));
    }

    #[cfg(unix)]
    #[test]
    fn a_synced_folder_reached_through_a_link_is_not_offered() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("real/Team")).unwrap();
        std::os::unix::fs::symlink(root.path().join("real"), root.path().join("via")).unwrap();
        let next = root.path().join("via/Team 2").display().to_string();
        let synced = root.path().join("real/Team").display().to_string();
        assert_eq!(sync_existing_folder("Team".into(), next, vec![synced]), None);
    }

    #[test]
    fn the_share_menu_finds_the_innermost_synced_folder() {
        let root = tempfile::tempdir().unwrap();
        let outer = root.path().join("Work");
        let inner = outer.join("Team");
        std::fs::create_dir_all(inner.join("Docs")).unwrap();
        let f = |name: &str, p: &Path| SyncFolder { path: p.display().to_string(), ..folder(name, SyncState::UpToDate) };
        let folders = vec![f("Work", &outer), f("Team", &inner)];
        let at = |p: PathBuf| sync_folder_containing(folders.clone(), p.display().to_string()).map(|f| f.name);
        assert_eq!(at(inner.join("Docs")).as_deref(), Some("Team"));
        assert_eq!(at(inner.clone()).as_deref(), Some("Team"));
        assert_eq!(at(outer.clone()).as_deref(), Some("Work"));
        assert_eq!(at(root.path().join("Work2")), None, "a name that only starts the same is not inside");
        assert_eq!(at(root.path().to_path_buf()), None);
        let reader = SyncFolder { role: SyncRole::ReadOnly, ..folder("Photos", SyncState::UpToDate) };
        let r = sync_share_refusal(reader).unwrap();
        assert_eq!(r.title, "Only the owner and admins can share “Photos”");
        assert_eq!(r.text, format!("This {} is Read only in it. Ask the owner or an admin for a link.", computer!()));
        assert_eq!(sync_share_refusal(folder("Photos", SyncState::UpToDate)), None);
    }

    #[test]
    fn errors_and_alerts_read_as_sentences() {
        assert_eq!(core_error_message(CoreError::Failed { message: "No space left.".into() }), "No space left.");
        assert_eq!(core_error_message(CoreError::Invalid { problems: vec!["a: x".into(), "b: y".into()] }), "a: x\nb: y");
        assert_eq!(
            core_error_message(CoreError::NotFound { id: "F".into() }),
            format!("This folder is no longer synced on this {}.", computer!())
        );
        assert_eq!(sync_remove_question("Photos".into()), format!("Stop syncing “Photos” on this {}? Other devices keep their copy.", computer!()));
        assert_eq!(sync_remove_trash_label(), concat!("Also move the files to the ", trash!()));
        assert_eq!(sync_remove_help(true), concat!("Stop syncing and move the folder to the ", trash!()));
        assert_eq!(sync_remove_help(false), "Stop syncing; the files stay");
        assert_eq!(alert_ok_title(), "OK");
        assert_eq!(sync_remove_failed("Photos".into(), "Busy.".into()), SyncAlert { title: "Couldn't stop syncing “Photos”".into(), text: "Busy.".into() });
        let t = sync_trash_failed("Photos".into(), "~/Seedwell/Photos".into());
        assert_eq!(t.title, concat!("Couldn't move “Photos” to the ", trash!()));
        assert_eq!(t.text, format!("It is no longer synced on this {}. Its files are still in ~/Seedwell/Photos.", computer!()));
        let other = TrashError::OtherVolume { trash: "/Users/me/.Trash".into() };
        assert_eq!(
            sync_trash_refusal("Photos", &other),
            concat!(
                "“Photos” is on another volume than the ", trash!(), ", so it can't be moved there. Turn off “Also move the files to the ", trash!(),
                "” to stop syncing it, then delete the folder in ", file_manager!(), "."
            )
        );
        let bin = TrashError::WouldDelete { reason: "the drive D:\\ has no Recycle Bin".into() };
        assert!(sync_trash_refusal("Photos", &bin).starts_with(concat!("The ", trash!(), " would delete “Photos” at once instead of keeping it (the drive D:\\ has no Recycle Bin). Turn off")));
        assert_eq!(sync_trash_refusal("Photos", &TrashError::NoTrash), concat!("There is no ", trash!(), " to move “Photos” to. Turn off “Also move the files to the ", trash!(), "” to stop syncing it."));
        let io = TrashError::Io(std::io::Error::other("Permission denied"));
        assert!(sync_trash_refusal("Photos", &io).starts_with(concat!("“Photos” can't be moved to the ", trash!(), " (Permission denied). Turn off")));
        assert_eq!(sync_resolve_failed("a.txt".into(), "x".into()).title, "Couldn't resolve the conflict in “a.txt”");
        assert_eq!(sync_set_role_failed("Kari".into(), "x".into()).title, "Couldn't change Kari’s role");
        assert_eq!(sync_remove_device_failed("Kari".into(), "Photos".into(), "x".into()).title, "Couldn't remove Kari from “Photos”");
        assert_eq!(
            sync_share_new_note("~/Novel".into()),
            format!("~/Novel is not synced yet. Copy Link starts syncing it from this {}, with you as its owner, and makes the link.", computer!())
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_mac_words_of_removing_a_folder_are_the_ones_it_always_had() {
        assert_eq!(sync_remove_trash_label(), "Also move the files to the Trash");
        assert_eq!(sync_remove_help(true), "Stop syncing and move the folder to the Trash");
        let t = sync_trash_failed("Photos".into(), "~/Seedwell/Photos".into());
        assert_eq!((t.title.as_str(), t.text.as_str()), ("Couldn't move “Photos” to the Trash", "It is no longer synced on this Mac. Its files are still in ~/Seedwell/Photos."));
    }
}
