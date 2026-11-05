//! Settings › Devices and the "On all my devices" toggles (SPEC §7G.1 "Your devices", §7G.5): every word, state and
//! choice the hosts show for the user's own devices, built here from the service's facts; the hosts only draw them.
//!
//! These calls are the app's own: no method of the agent interface reaches them (an agent never pairs, confirms,
//! removes, follows, leaves or starts over, at any level), and nothing here shows the user key.

use trs_platform::computer;
use trs_sync_net::SyncHandle;
use trs_sync_net::devices::{DevicesState, DevicesStatus, FollowState, RemovePlan};
use trs_sync_proto::ids::{DeviceId, FolderId};
use trs_sync_proto::members::{InviteLink, Role};

use crate::backend::Backend;
use crate::Core;
use trs_control::display as d;
use crate::sync::readable;
use crate::types::CoreError;

/// Where this device stands with its own devices.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum DevicesKind {
    /// In no group.
    Unpaired,
    /// In a group of its own; no other device paired yet.
    Alone,
    /// Paired with other devices.
    Paired,
    /// Joining with a pairing link; the other device's user confirms.
    Joining,
    /// Removed from its group: only Start Over helps.
    Removed,
}

/// One device in Settings › Devices.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct DeviceRow {
    pub id: String,
    /// "Studio PC" ("This Mac" when it has no name).
    pub title: String,
    /// "Kim · Online · paired by Anna's Mac", "Last seen 3 days ago".
    pub detail: String,
    pub is_me: bool,
    /// Remove… is offered (every device but this one).
    pub removable: bool,
    /// "Paired by “Old Laptop”, which was removed: keep it only if you paired it yourself."
    pub warning: Option<String>,
}

/// A device asking to pair with this one's link: the confirmation.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct PairRequestRow {
    pub id: String,
    /// "“Studio PC” wants to join your devices."
    pub question: String,
    /// The id the other device shows, in groups: "MFZWI3D-BONSGYC-YLTMRWG-QOEKXSA".
    pub id_line: String,
    /// "Pair only if “Studio PC” shows this id. It can then reach every folder and channel you put on all your devices."
    pub hint: String,
    pub pair_title: String,
    pub refuse_title: String,
}

/// Settings › Devices.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct DevicesView {
    pub kind: DevicesKind,
    /// "Your devices trust each other: …"
    pub intro: String,
    pub rows: Vec<DeviceRow>,
    pub requests: Vec<PairRequestRow>,
    /// "Pair a Device…": offered unless this device is joining or was removed.
    pub pair_title: String,
    pub pair_enabled: bool,
    /// "Pair with a Link…": offered unless this device is paired with others or joining.
    pub link_title: String,
    pub link_enabled: bool,
    /// "Leave Your Devices…" while paired (or alone), "Start Over…" once removed; `None`: neither.
    pub leave_title: Option<String>,
    /// The leave (or start over) confirmation's question.
    pub leave_question: String,
    /// The confirmation's button: "Leave" or "Start Over".
    pub leave_action: String,
    /// Joining: what it does now ("Confirm on “Anna's Mac”: …"), the last problem, and this device's id to compare.
    pub joining_line: Option<String>,
    pub joining_problem: Option<String>,
    pub my_id_line: String,
    /// Under the list: why a second device as co-owner is a good backup.
    pub note: String,
    /// While Settings › Devices is shown: read the view again this often (the list, requests, a link's countdown).
    pub refresh_ms: u32,
}

/// A pairing link this device made, for Pair a Device….
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct PairLinkView {
    pub link: String,
    pub expires_s: u64,
    pub title: String,
    /// "Paste this link on your other device (Settings › Devices › Pair with a Link…). …"
    pub text: String,
    pub copy_title: String,
}

/// One folder or channel the removal also removes the device from.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct RemovePlaceRow {
    pub id: String,
    /// "Dev (folder)", "Seedwell Development (channel)".
    pub title: String,
    pub ticked: bool,
}

/// Remove… of a device.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct RemoveDeviceView {
    pub device_id: String,
    /// "Remove “Old Laptop” from your devices?"
    pub title: String,
    pub text: String,
    /// "Also remove it from:" and the places where this device is owner, ticked.
    pub places_title: String,
    pub places: Vec<RemovePlaceRow>,
    /// "Ask the owner of “Team” to remove it there."
    pub ask_owner: Option<String>,
    /// "It paired “Phone”: remove that one too if you did not pair it yourself."
    pub paired: Option<String>,
    pub remove_title: String,
}

/// The "On all my devices" toggle of a folder or channel.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct FollowLine {
    /// Shown at all (this device is paired with others, or the item is on all devices already).
    pub shown: bool,
    /// "On all my devices" (a toggle's), and "On All My Devices" (a menu item's).
    pub title: String,
    pub menu_title: String,
    pub on: bool,
    /// The toggle can be switched (on needs Owner or Admin here; off is always allowed).
    pub enabled: bool,
    /// "Your other devices (MacBook, Studio PC) join it by themselves, as admins." or why it cannot be switched on.
    pub line: String,
    /// The toggle's tooltip: "Put it on every device paired with this Mac: they join it by themselves".
    pub help: String,
    /// A folder's "Read only" toggle under it (`None` for a channel, and while the toggle is not shown).
    pub read_only: Option<FollowReadOnly>,
}

/// "Read only" under a folder's "On all my devices" (SPEC §7G.1): the user's other devices open and download its files
/// but never change, add or delete anything, here or on each other, and nothing is downloaded to this computer.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct FollowReadOnly {
    /// "Read only".
    pub title: String,
    /// As the core has it; in a dialog's model, the form's choice.
    pub on: bool,
    /// It can be switched: owner or admin here (and in a dialog, while "On all my devices" is chosen).
    pub enabled: bool,
    /// What it means as it is set, and what changes when it is applied ("Your other devices that have it become Read
    /// only…").
    pub line: String,
    /// Its tooltip.
    pub help: String,
    /// This computer is an owner of the folder: the roles change at once. Else they change once an owner is online
    /// (the line says so).
    pub owner: bool,
}

/// A toggle with the fixed titles and tooltip (`read_only`: a folder's, as the core has it).
fn follow(shown: bool, on: bool, enabled: bool, line: String, read_only: Option<FollowReadOnly>) -> FollowLine {
    FollowLine {
        shown,
        title: "On all my devices".into(),
        menu_title: MENU_TITLE.into(),
        on,
        enabled,
        line,
        help: concat!("Put it on every device paired with this ", computer!(), ": they join it by themselves").into(),
        read_only: read_only.filter(|_| shown),
    }
}

/// The "Read only" toggle as the core has it: `on`, `may` (owner or admin here) for whether it can be switched, and
/// whether the folder is on all devices (`follow_on`), and whether this computer is an owner of it (`owner`).
fn read_only_line(on: bool, may: bool, follow_on: bool, owner: bool) -> FollowReadOnly {
    let r = FollowReadOnly { title: "Read only".into(), on, enabled: may, line: String::new(), help: READ_ONLY_HELP.into(), owner };
    // `enabled` stays `may`: a dialog draws it from here with its own choice of "On all my devices".
    FollowReadOnly { line: follow_read_only_drawn(&r, on, follow_on, follow_on).line, ..r }
}

/// The "Read only" toggle drawn for a dialog: `core` as the core has it, `chosen` the form's choice, `follow_on` whether
/// "On all my devices" is on now and `follow_chosen` whether it is chosen. Its line says what it means and what Save
/// (or the switch, where it applies at once) changes. Without "On all my devices" chosen it cannot be switched and
/// stays as it is.
pub(crate) fn follow_read_only_drawn(core: &FollowReadOnly, chosen: bool, follow_on: bool, follow_chosen: bool) -> FollowReadOnly {
    let what = "can open and download its files, but never change, add or delete anything, here or on each other";
    let here = concat!("Nothing is downloaded to this ", computer!());
    // Only an owner changes roles: from an admin, what the line promises waits for one (devices on this version hold
    // back meanwhile; one on an older Seedwell does not).
    let wait_on = if core.owner { "" } else { " Until an owner of this folder is online, a device on an older Seedwell can still send changes." };
    let wait_off = if core.owner { "" } else { " Their roles change once an owner of this folder is online." };
    let chosen = if follow_chosen { chosen } else { core.on };
    let line = if !core.enabled {
        if core.on {
            "Your devices open and download its files but change nothing in it. Only an owner or admin of this folder can switch it.".to_string()
        } else {
            "Only an owner or admin of this folder can switch it.".to_string()
        }
    } else if !follow_chosen {
        if core.on { "Your other devices that have it stay Read only.".to_string() } else { String::new() }
    } else {
        match (core.on, chosen) {
            (true, true) => format!("Your other devices {what}. {here}.{wait_on}"),
            (false, true) if follow_on => format!("Your other devices that have it become Read only: they {what}. {here}, which keeps its role.{wait_on}"),
            (false, true) => format!("Your other devices join it Read only: they {what}. {here}.{wait_on}"),
            (true, false) => format!("Your other devices become admins again: they can change, add and delete its files, here and on each other.{wait_off}"),
            (false, false) => "Off: your other devices join it as admins and can change, add and delete its files.".to_string(),
        }
    };
    FollowReadOnly { on: chosen, enabled: core.enabled && follow_chosen, line, ..core.clone() }
}

/// The "Read only" toggle's tooltip.
const READ_ONLY_HELP: &str = concat!(
    "Your other devices can open and download its files but never change, add or delete anything, here or on each other; nothing is downloaded to this ",
    computer!()
);

/// The "On all my devices" toggle of folder or channel `id` as `backend` has it (hidden while this computer is not
/// paired).
pub(crate) fn follow_line_for(backend: &dyn Backend, id: &str) -> FollowLine {
    let hidden = follow(false, false, false, String::new(), None);
    let Some((h, _)) = backend.sync().service() else { return hidden };
    let Ok(fid) = id.parse::<FolderId>() else { return hidden };
    let channel = h.channel_status(&fid).is_some();
    h.follow_state(&fid).map(|f| follow_line_of(&f, channel)).unwrap_or(hidden)
}

// ------------------------------------------------------------------------------------------------- texts

/// The channel menu's item.
const MENU_TITLE: &str = "On All My Devices";

/// The intro line of Settings › Devices.
const INTRO: &str = "Your devices trust each other: folders and channels you put on all your devices come to each of them by themselves.";

/// The first four groups of a device id: what the two screens compare.
fn id_line(id: &DeviceId) -> String {
    id.to_string().split('-').take(4).collect::<Vec<_>>().join("-")
}

fn quoted_list(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => one.clone(),
        [first @ .., last] => format!("{} and {last}", first.join(", ")),
    }
}

/// Settings › Devices for `s`, at `now_s`.
pub(crate) fn devices_view_of(s: &DevicesStatus, now_s: u64) -> DevicesView {
    let kind = match s.state {
        DevicesState::None => DevicesKind::Unpaired,
        DevicesState::Alone => DevicesKind::Alone,
        DevicesState::Paired => DevicesKind::Paired,
        DevicesState::Joining => DevicesKind::Joining,
        DevicesState::Removed => DevicesKind::Removed,
    };
    let rows = s
        .devices
        .iter()
        .map(|dv| {
            let title = if dv.name.is_empty() { concat!("This ", computer!()).to_string() } else { dv.name.clone() };
            let mut parts: Vec<String> = Vec::new();
            if dv.is_me {
                parts.push(concat!("This ", computer!()).into());
            }
            if !dv.user.is_empty() {
                parts.push(dv.user.clone());
            }
            if !dv.is_me {
                parts.push(match (dv.online, dv.last_seen_s) {
                    (true, _) => "Online".into(),
                    (false, Some(t)) => format!("Last seen {}", d::ago(t, now_s, false).to_lowercase()),
                    (false, None) => "Offline".into(),
                });
            }
            if !dv.paired_by_name.is_empty() && dv.paired_by != Some(s.me) {
                parts.push(format!("paired by {}", dv.paired_by_name));
            }
            let warning = dv.paired_by_removed.then(|| {
                let by = if dv.paired_by_name.is_empty() { "a device".to_string() } else { format!("“{}”", dv.paired_by_name) };
                format!("Paired by {by}, which was removed: keep it only if you paired it yourself.")
            });
            DeviceRow { id: dv.id.to_string(), title, detail: parts.join(" · "), is_me: dv.is_me, removable: !dv.is_me, warning }
        })
        .collect();
    let requests = s
        .requests
        .iter()
        .map(|r| {
            let name = if r.name.is_empty() { "A device".to_string() } else { format!("“{}”", r.name) };
            let them = if r.name.is_empty() { "the other device".to_string() } else { format!("“{}”", r.name) };
            PairRequestRow {
                id: r.id.to_string(),
                question: format!("{name} wants to join your devices."),
                id_line: id_line(&r.id),
                hint: format!("Pair only if {them} shows this id. It can then reach every folder and channel you put on all your devices."),
                pair_title: "Pair".into(),
                refuse_title: "Refuse".into(),
            }
        })
        .collect();
    let joining = s.state == DevicesState::Joining;
    let (leave_title, leave_question) = match s.state {
        DevicesState::Removed => (
            Some("Start Over…".to_string()),
            concat!(
                "Start over with new keys? This ",
                computer!(),
                " leaves its old group of devices; pair your other devices with it again. Folders and channels stay."
            )
            .to_string(),
        ),
        DevicesState::Paired | DevicesState::Alone | DevicesState::Joining => (
            Some("Leave Your Devices…".to_string()),
            concat!(
                "Leave your devices? This ",
                computer!(),
                " stops being one of them, and nothing comes to it by itself any more. Folders and channels stay."
            )
            .to_string(),
        ),
        DevicesState::None => (None, String::new()),
    };
    DevicesView {
        kind,
        intro: INTRO.into(),
        rows,
        requests,
        pair_title: "Pair a Device…".into(),
        pair_enabled: matches!(s.state, DevicesState::None | DevicesState::Alone | DevicesState::Paired),
        link_title: "Pair with a Link…".into(),
        link_enabled: matches!(s.state, DevicesState::None | DevicesState::Alone | DevicesState::Removed),
        leave_title,
        leave_question,
        leave_action: if s.state == DevicesState::Removed { "Start Over" } else { "Leave" }.into(),
        joining_line: if joining { s.progress.clone() } else { None },
        joining_problem: if joining { s.problem.as_deref().map(readable) } else { None },
        my_id_line: format!(concat!("This ", computer!(), "'s id: {}"), id_line(&s.me)),
        note: "Folders and channels you put on all your devices come to the others with them as admins. Make a second device \
               co-owner in a folder's sharing too: then the folder can still be managed if this one is lost."
            .into(),
        refresh_ms: 1_000,
    }
}

/// The text of Pair a Device….
pub(crate) fn pair_link_view(link: String, expires_s: u64) -> PairLinkView {
    PairLinkView {
        link,
        expires_s,
        title: "Pair a Device".into(),
        text: "Paste this link on your other device, in Settings › Devices › Pair with a Link…. It works once, for 15 minutes. \
               Then confirm here: this window shows the other device's id."
            .into(),
        copy_title: "Copy".into(),
    }
}

/// "Expires in 14:32" (or "Expired").
#[uniffi::export]
pub fn devices_countdown(expires_s: u64, now_s: u64) -> String {
    if now_s >= expires_s {
        return "Expired: make a new link".into();
    }
    let left = expires_s - now_s;
    format!("Expires in {}:{:02}", left / 60, left % 60)
}

/// Remove… for `plan`.
pub(crate) fn remove_view_of(p: &RemovePlan) -> RemoveDeviceView {
    let name = if p.name.is_empty() { "this device".to_string() } else { format!("“{}”", p.name) };
    let place = |name: &str, channel: bool| format!("{name} ({})", if channel { "channel" } else { "folder" });
    let ask_owner = (!p.admin.is_empty()).then(|| {
        let names: Vec<String> = p.admin.iter().map(|a| format!("“{}”", a.name)).collect();
        format!("Ask the owner of {} to remove it there.", quoted_list(&names))
    });
    let paired = (!p.paired.is_empty()).then(|| {
        let names: Vec<String> = p.paired.iter().map(|(_, n)| format!("“{n}”")).collect();
        format!("It paired {}: remove {} too if you did not pair {} yourself.", quoted_list(&names), if names.len() == 1 { "that one" } else { "those" }, if names.len() == 1 { "it" } else { "them" })
    });
    RemoveDeviceView {
        device_id: p.id.to_string(),
        title: format!("Remove {name} from your devices?"),
        text: "It can never join them again, and none of them admits it anywhere again: your devices change their key, so \
               the one it holds is worth nothing. What it already has stays with it."
            .into(),
        places_title: "Also remove it from:".into(),
        places: p.owner.iter().map(|x| RemovePlaceRow { id: x.id.to_string(), title: place(&x.name, x.channel), ticked: true }).collect(),
        ask_owner,
        paired,
        remove_title: "Remove".into(),
    }
}

/// The toggle of a folder or channel for `f` (`channel`: its noun). A folder's line leaves the role to its "Read only"
/// toggle's line; a channel's says "as admins".
pub(crate) fn follow_line_of(f: &FollowState, channel: bool) -> FollowLine {
    let noun = if channel { "channel" } else { "folder" };
    let others = quoted_list(&f.others);
    let role = if channel { ", as admins" } else { "" };
    let line = if !f.paired {
        concat!("Pair this ", computer!(), " with your other devices first (Settings › Devices).").to_string()
    } else if !f.role.is_some_and(Role::admits) && !f.on {
        format!("Only an owner or admin of this {noun} can put it on all your devices: your other devices could not be admitted.")
    } else if f.on {
        format!("Your other devices ({others}) have it or join it by themselves{role}. Turning it off deletes nothing.")
    } else {
        format!("Your other devices ({others}) join it by themselves{role}.")
    };
    let read_only = (!channel).then(|| read_only_line(f.read_only, f.may, f.on, f.role == Some(Role::Owner)));
    follow(f.paired || f.on, f.on, f.on || f.may, line, read_only)
}

/// The toggle in Add Folder and New Channel, before the item exists: on by default for a channel, off for a folder
/// (and so is a folder's "Read only").
pub(crate) fn follow_line_new(others: &[String], channel: bool) -> FollowLine {
    let shown = !others.is_empty();
    let role = if channel { ", as admins" } else { "" };
    let line = if shown { format!("Your other devices ({}) join it by themselves{role}.", quoted_list(others)) } else { String::new() };
    // A folder added here is this computer's own.
    let read_only = (!channel).then(|| read_only_line(false, shown, false, true));
    follow(shown, shown && channel, shown, line, read_only)
}

fn devices_err(e: trs_sync_net::SyncError) -> CoreError {
    match e {
        trs_sync_net::SyncError::NotFound => CoreError::Failed { message: "That device is no longer one of your devices.".into() },
        other => CoreError::Failed { message: readable(&trs_control::redact::scrub_keys(&other.to_string())) },
    }
}

fn not_running() -> CoreError {
    CoreError::Failed { message: "Folder sync is still starting. Try again in a moment.".into() }
}

fn device_id(id: &str) -> Result<DeviceId, CoreError> {
    id.parse().map_err(|_| CoreError::Failed { message: "That device is no longer one of your devices.".into() })
}

fn now_s() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |t| t.as_secs())
}

impl Core {
    fn devices_service(&self) -> Result<(SyncHandle, tokio::runtime::Handle), CoreError> {
        self.backend.sync().service().ok_or_else(not_running)
    }
}

/// The app's own calls for the user's devices (never reachable by agents).
impl Core {
    /// Pair a Device…: a one-use link for 15 minutes (the group is made now if there is none). The link is shown to the
    /// user only.
    pub fn devices_pair_link(&self) -> Result<PairLinkView, CoreError> {
        let (h, rt) = self.devices_service()?;
        let (link, expires) = rt.block_on(h.devices_pair_link()).map_err(devices_err)?;
        Ok(pair_link_view(link, expires))
    }

    /// Pair with a Link…: joins the devices of the device that made `link`; its user confirms there.
    pub fn devices_join(&self, link: String) -> Result<DevicesView, CoreError> {
        let (h, rt) = self.devices_service()?;
        let s = rt.block_on(h.devices_join(&link)).map_err(devices_err)?;
        Ok(devices_view_of(&s, now_s()))
    }

    /// Pair: admits the device asking with this Mac's link.
    pub fn devices_confirm(&self, device_id: String) -> Result<(), CoreError> {
        let (h, rt) = self.devices_service()?;
        rt.block_on(h.devices_confirm(&self::device_id(&device_id)?)).map_err(devices_err)
    }

    /// Refuse: the device asking is not admitted, and the link admits nobody any more.
    pub fn devices_refuse(&self, device_id: String) -> Result<(), CoreError> {
        let (h, rt) = self.devices_service()?;
        rt.block_on(h.devices_refuse(&self::device_id(&device_id)?)).map_err(devices_err)
    }

    /// Remove… of a device: what it does.
    pub fn devices_remove_view(&self, device_id: String) -> Result<RemoveDeviceView, CoreError> {
        let (h, _) = self.devices_service()?;
        let p = h.devices_remove_plan(&self::device_id(&device_id)?).map_err(devices_err)?;
        Ok(remove_view_of(&p))
    }

    /// Removes a device from the user's devices for good, and from the ticked places (`places`: their ids).
    pub fn devices_remove(&self, device_id: String, places: Vec<String>) -> Result<(), CoreError> {
        let (h, rt) = self.devices_service()?;
        let places: Vec<FolderId> = places.iter().filter_map(|p| p.parse().ok()).collect();
        rt.block_on(h.devices_remove(&self::device_id(&device_id)?, &places)).map_err(devices_err)
    }

    /// Leave Your Devices….
    pub fn devices_leave(&self) -> Result<(), CoreError> {
        let (h, rt) = self.devices_service()?;
        rt.block_on(h.devices_leave()).map_err(devices_err)
    }

    /// Start Over…: leaves and makes a new group with this Mac alone.
    pub fn devices_start_over(&self) -> Result<DevicesView, CoreError> {
        let (h, rt) = self.devices_service()?;
        let s = rt.block_on(h.devices_start_over()).map_err(devices_err)?;
        Ok(devices_view_of(&s, now_s()))
    }

    /// Turns "On all my devices" on or off for folder or channel `id`.
    pub fn sync_set_follow(&self, id: String, on: bool) -> Result<(), CoreError> {
        let (h, rt) = self.devices_service()?;
        let fid: FolderId = id.parse().map_err(|_| CoreError::Failed { message: concat!("This folder is no longer synced on this ", computer!(), ".").into() })?;
        rt.block_on(h.set_follow(&fid, on)).map_err(devices_err)
    }

    /// Turns "Read only" on or off for folder `id` on the user's other devices (on also puts it on all of them).
    pub fn sync_set_follow_read_only(&self, id: String, read_only: bool) -> Result<(), CoreError> {
        let (h, rt) = self.devices_service()?;
        let fid: FolderId = id.parse().map_err(|_| CoreError::Failed { message: concat!("This folder is no longer synced on this ", computer!(), ".").into() })?;
        rt.block_on(h.set_follow_read_only(&fid, read_only)).map_err(devices_err)
    }
}

#[uniffi::export]
impl Core {
    /// Settings › Devices. Cheap; called once a second while the tab is open.
    pub fn devices_view(&self) -> DevicesView {
        let status = self.backend.sync().service().map(|(h, _)| h.devices_status());
        match status {
            Some(s) => devices_view_of(&s, now_s()),
            None => devices_view_of(
                &DevicesStatus { state: DevicesState::None, me: DeviceId([0; 32]), devices: Vec::new(), requests: Vec::new(), link: None, progress: None, problem: None, maker: None },
                now_s(),
            ),
        }
    }

    /// The pairing link this Mac made that still admits, to show again (`None`: make a new one).
    pub fn devices_current_link(&self) -> Option<PairLinkView> {
        let (h, _) = self.backend.sync().service()?;
        h.devices_status().link.map(|(l, e)| pair_link_view(l, e))
    }

    /// Why a pasted text is not a pairing link, as a sentence (`None`: it is one, or the field is empty).
    pub fn devices_link_problem(&self, text: String) -> Option<String> {
        if text.trim().is_empty() {
            return None;
        }
        match InviteLink::parse_devices(&text) {
            Ok(l) if l.invite.expired(now_s()) => Some("This link has expired: make a new one on the other device.".into()),
            Ok(_) => None,
            Err(e) => Some(readable(&e.to_string())),
        }
    }

    /// The "On all my devices" toggle of folder or channel `id` (hidden while this Mac is not paired).
    pub fn sync_follow_line(&self, id: String) -> FollowLine {
        follow_line_for(self.backend.as_ref(), &id)
    }

    /// The toggle in Add Folder (`channel` false) and New Channel, before the item exists.
    pub fn sync_follow_line_new(&self, channel: bool) -> FollowLine {
        let others: Vec<String> = self
            .backend
            .sync()
            .service()
            .map(|(h, _)| h.devices_status())
            .filter(|s| s.state == DevicesState::Paired)
            .map(|s| s.devices.into_iter().filter(|d| !d.is_me).map(|d| d.name).filter(|n| !n.is_empty()).collect())
            .unwrap_or_default();
        follow_line_new(&others, channel)
    }

}

#[cfg(test)]
mod tests {
    use trs_sync_net::devices::{OwnDevice, PairRequestInfo, Place};

    use super::*;

    fn dev(n: u8, name: &str, me: bool) -> OwnDevice {
        OwnDevice {
            id: DeviceId([n; 32]),
            name: name.into(),
            user: if me { "Kim".into() } else { String::new() },
            is_me: me,
            online: false,
            last_seen_s: None,
            paired_by: None,
            paired_by_name: String::new(),
            paired_by_removed: false,
        }
    }

    fn status(state: DevicesState, devices: Vec<OwnDevice>) -> DevicesStatus {
        DevicesStatus { state, me: DeviceId([1; 32]), devices, requests: Vec::new(), link: None, progress: None, problem: None, maker: None }
    }

    #[test]
    fn the_list_says_who_is_online_and_who_paired_whom() {
        let mut studio = dev(2, "Studio PC", false);
        studio.online = true;
        studio.paired_by = Some(DeviceId([1; 32]));
        studio.paired_by_name = "Anna's Mac".into();
        let mut phone = dev(3, "Phone", false);
        phone.last_seen_s = Some(1_000);
        phone.paired_by = Some(DeviceId([9; 32]));
        phone.paired_by_name = "Old Laptop".into();
        phone.paired_by_removed = true;
        let v = devices_view_of(&status(DevicesState::Paired, vec![dev(1, "Anna's Mac", true), studio, phone]), 1_000 + 3 * 86_400);
        assert_eq!(v.kind, DevicesKind::Paired);
        assert_eq!(v.intro, INTRO);
        assert_eq!(v.rows[0].detail, concat!("This ", computer!(), " · Kim"));
        assert!(!v.rows[0].removable && v.rows[1].removable);
        assert_eq!(v.rows[1].detail, "Online", "paired by this device: not said");
        assert!(v.rows[2].detail.starts_with("Last seen ") && v.rows[2].detail.ends_with("paired by Old Laptop"), "{}", v.rows[2].detail);
        assert_eq!(v.rows[2].warning.as_deref(), Some("Paired by “Old Laptop”, which was removed: keep it only if you paired it yourself."));
        assert!(v.pair_enabled && !v.link_enabled);
        assert_eq!(v.leave_title.as_deref(), Some("Leave Your Devices…"));
    }

    #[test]
    fn a_request_shows_the_id_to_compare_and_the_buttons_follow_the_state() {
        let mut s = status(DevicesState::Alone, vec![dev(1, "Anna's Mac", true)]);
        s.requests.push(PairRequestInfo { id: DeviceId([7; 32]), name: "Studio PC".into(), at_s: 5, expires_s: 900 });
        let v = devices_view_of(&s, 10);
        let r = &v.requests[0];
        assert_eq!(r.question, "“Studio PC” wants to join your devices.");
        assert_eq!(r.id_line, id_line(&DeviceId([7; 32])));
        assert_eq!(r.id_line.split('-').count(), 4);
        assert!(r.hint.starts_with("Pair only if “Studio PC” shows this id."));
        assert_eq!((r.pair_title.as_str(), r.refuse_title.as_str()), ("Pair", "Refuse"));
        assert!(v.pair_enabled && v.link_enabled, "alone: either way");
        let mut joining = status(DevicesState::Joining, Vec::new());
        joining.progress = Some("Confirm on “Anna's Mac”: it shows this Mac's id".into());
        joining.problem = Some("this link has been used up: make a new one on the other device".into());
        let v = devices_view_of(&joining, 10);
        assert!(!v.pair_enabled && !v.link_enabled);
        assert_eq!(v.joining_problem.as_deref(), Some("This link has been used up: make a new one on the other device."));
        assert!(v.my_id_line.ends_with(&id_line(&DeviceId([1; 32]))));
        let v = devices_view_of(&status(DevicesState::Removed, Vec::new()), 10);
        assert_eq!(v.leave_title.as_deref(), Some("Start Over…"));
        assert!(v.leave_question.starts_with("Start over with new keys?"));
        assert!(!v.pair_enabled && v.link_enabled);
        assert_eq!(devices_view_of(&status(DevicesState::None, Vec::new()), 10).leave_title, None);
    }

    #[test]
    fn the_countdown_counts_minutes_and_seconds() {
        assert_eq!(devices_countdown(1_000 + 872, 1_000), "Expires in 14:32");
        assert_eq!(devices_countdown(1_000, 1_000), "Expired: make a new link");
        assert!(pair_link_view("seedwell://devices/x".into(), 5).text.contains("15 minutes"));
    }

    #[test]
    fn the_removal_lists_the_owners_places_ticked_and_asks_for_the_rest() {
        let place = |n: u8, name: &str, channel: bool| Place { id: FolderId([n; 32]), name: name.into(), channel };
        let p = RemovePlan {
            id: DeviceId([4; 32]),
            name: "Old Laptop".into(),
            owner: vec![place(1, "Dev", false), place(2, "Seedwell Development", true)],
            admin: vec![place(3, "Team", false)],
            paired: vec![(DeviceId([5; 32]), "Phone".into())],
        };
        let v = remove_view_of(&p);
        assert_eq!(v.title, "Remove “Old Laptop” from your devices?");
        assert_eq!(v.places.iter().map(|x| (x.title.as_str(), x.ticked)).collect::<Vec<_>>(), [("Dev (folder)", true), ("Seedwell Development (channel)", true)]);
        assert_eq!(v.ask_owner.as_deref(), Some("Ask the owner of “Team” to remove it there."));
        assert_eq!(v.paired.as_deref(), Some("It paired “Phone”: remove that one too if you did not pair it yourself."));
        assert!(v.text.contains("change their key") && !v.text.contains("Start Over"), "{}", v.text);
    }

    #[test]
    fn the_toggle_says_who_joins_or_why_it_cannot() {
        let f = |on, may, paired, role| FollowState { on, may, paired, role, others: vec!["MacBook".into(), "Studio PC".into()], read_only: false };
        let l = follow_line_of(&f(false, true, true, Some(Role::Owner)), false);
        assert!(l.shown && l.enabled && !l.on);
        assert_eq!(l.line, "Your other devices (MacBook and Studio PC) join it by themselves.");
        assert!(follow_line_of(&f(false, true, true, Some(Role::Owner)), true).line.ends_with("as admins."), "a channel says the role");
        let l = follow_line_of(&f(false, false, true, Some(Role::ReadWrite)), true);
        assert!(l.shown && !l.enabled && l.line.starts_with("Only an owner or admin of this channel"));
        let l = follow_line_of(&f(true, false, true, Some(Role::ReadWrite)), false);
        assert!(l.enabled && l.on, "off is always allowed");
        let l = follow_line_of(&f(false, false, false, Some(Role::Owner)), false);
        assert!(!l.shown && !l.enabled);
        let n = follow_line_new(&["MacBook".into()], true);
        assert!(n.shown && n.on && n.enabled, "a new channel: on by default when paired");
        assert!(!follow_line_new(&["MacBook".into()], false).on, "a new folder: off");
        assert!(!follow_line_new(&[], true).shown, "not paired: not shown");
    }

    /// The "Read only" toggle must say what applying it does before it is applied (that the devices which have the
    /// folder lose write access, that turning it off makes them admins again), never offer a switch that cannot be
    /// applied (a member that is not owner or admin, "On all my devices" not chosen), and never show for a channel.
    #[test]
    /// Review of Read only, #5: on an admin the line promised "become Read only" while nothing changed until an owner
    /// was online. Fails when an admin's line does not say the roles wait for an owner, or an owner's says so.
    fn the_read_only_line_of_an_admin_says_the_roles_wait_for_an_owner() {
        let f = |role| FollowState { on: true, may: true, paired: true, role: Some(role), others: vec!["MacBook".into()], read_only: false };
        let admin = follow_line_of(&f(Role::Admin), false).read_only.unwrap();
        assert!(!admin.owner);
        let on = follow_read_only_drawn(&admin, true, true, true).line;
        assert!(on.contains("become Read only") && on.contains("Until an owner of this folder is online"), "{on}");
        let off = follow_read_only_drawn(&FollowReadOnly { on: true, ..admin }, false, true, true).line;
        assert!(off.contains("once an owner of this folder is online"), "{off}");
        let owner = follow_line_of(&f(Role::Owner), false).read_only.unwrap();
        assert!(!follow_read_only_drawn(&owner, true, true, true).line.contains("owner of this folder is online"));
    }

    #[test]
    fn the_read_only_toggle_says_what_it_changes_and_only_offers_what_applies() {
        let f = |on, may, read_only| FollowState { on, may, paired: true, role: Some(if may { Role::Owner } else { Role::ReadOnly }), others: vec!["MacBook".into()], read_only };
        assert_eq!(follow_line_of(&f(true, true, false), true).read_only, None, "a channel has none");
        let core = follow_line_of(&f(true, true, false), false).read_only.unwrap();
        assert!(!core.on && core.enabled && core.title == "Read only" && core.help.contains("never change, add or delete"), "{core:?}");
        // Turning it on for a folder the other devices have: they become Read only, this computer keeps its role.
        let on = follow_read_only_drawn(&core, true, true, true);
        assert!(on.on && on.enabled && on.line.contains("that have it become Read only") && on.line.contains("keeps its role"), "{}", on.line);
        assert!(on.line.contains(concat!("Nothing is downloaded to this ", computer!())), "{}", on.line);
        // Put on all devices together with it: they join Read only.
        assert!(follow_read_only_drawn(&core, true, false, true).line.starts_with("Your other devices join it Read only"));
        // Turning it off: they become admins again.
        let was = follow_line_of(&f(true, true, true), false).read_only.unwrap();
        assert!(was.on && was.line.contains("never change"), "{}", was.line);
        let off = follow_read_only_drawn(&was, false, true, true);
        assert!(!off.on && off.line.starts_with("Your other devices become admins again"), "{}", off.line);
        // Without "On all my devices" chosen it cannot be switched, and stays as it is.
        let alone = follow_read_only_drawn(&was, false, true, false);
        assert!(alone.on && !alone.enabled && alone.line.contains("stay Read only"), "{alone:?}");
        // A Read only member sees it, cannot switch it, and is told who can.
        let member = follow_line_of(&f(true, false, true), false).read_only.unwrap();
        assert!(member.on && !follow_read_only_drawn(&member, false, true, true).enabled && member.line.contains("Only an owner or admin"), "{member:?}");
        // A new folder: off, switchable once On all my devices is chosen.
        let new = follow_line_new(&["MacBook".into()], false).read_only.unwrap();
        assert!(!new.on && !follow_read_only_drawn(&new, true, false, false).enabled && follow_read_only_drawn(&new, true, false, true).enabled);
    }
}
