//! The Folder Sync page of the main window (SPEC 7G.5) as a view model every host draws (docs/CROSS-PLATFORM.md §1):
//! the folders on the left with their context menus and Add Folder…, and the selected folder's page: its header and
//! buttons, where it is, its status, the devices gone and the Share Again prompt, the Stop Syncing question, and the
//! Members, Conflicts and Issues sections with their menus and the questions they ask first. Rust decides which items
//! there are (and leaves out what needs a window the host lacks: `HostWindow`), every word and what each one does; the
//! host draws, reads the members and conflicts for it, and runs the [`SyncPageCommand`]s.
//!
//! - [`sync_page_model`]: the page.
//! - [`SyncRemoval`] (sync_menu.rs): its Stop Syncing question, one of its own (not the menu's).

use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::Arc;

use trs_control::display as d;
use trs_platform::{computer, file_manager};

use crate::chat_text::named;
use crate::host::{has, HostFeature, HostWindow};
use crate::library::{local_day, LocalClock};
use crate::sync::{SyncChannel, SyncConflict, SyncDevice, SyncFolder, SyncKeep, SyncMember, SyncRole, SyncSnapshot, SyncState};
use crate::sync_menu::{add_folder_item, SyncMenuConfirm, SyncMenuItem, SyncRemoval};
use crate::sync_text::{
    badge, join_note, role, subtitle, sync_can_share, sync_gone_prompt, sync_member_line, sync_members_ordered, sync_on_demand_notice,
    sync_on_demand_off_line, sync_place_line, sync_remove_folder_question, sync_remove_help, sync_remove_trash_label, sync_reshare_prompt,
    SyncBadge,
};

const REVEAL: &str = concat!("Show in ", file_manager!());
const CANCEL: &str = "Cancel";
/// The members change as devices come and go (the core keeps them apart from the folder's index: a cheap read).
const MEMBERS_REFRESH_MS: u32 = 2_000;
/// Reading the conflict copies reads the folder's whole index.
const CONFLICTS_REFRESH_MS: u32 = 10_000;

// MARK: The records

/// What an item of the page does. The page's own items act on the folder shown ([`SyncFolderPage::folder_id`]), a
/// folder's context menu on that folder. After a change the host reads the members or conflicts again and asks for the
/// model again; a failed call is told in an alert (the `sync_*_failed` texts).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum SyncPageCommand {
    /// Shows the folder in the file manager: the host does what [`sync_reveal`] says (a missing folder: its page, as
    /// there is nothing to reveal).
    Reveal,
    /// Its Share dialog (`HostWindow::FolderShare`).
    Share,
    /// The page of its channel.
    OpenChat { channel_id: String },
    /// Its Edit dialog (`HostWindow::FolderEditor`).
    Edit,
    /// Selects the folder and opens its Stop Syncing question under the header: `SyncRemoval::open` with
    /// `Core::sync_only_here`.
    AskRemove,
    /// The question's Cancel: `SyncRemoval::close`.
    CancelRemove,
    /// The question's Remove: `SyncRemoval::close`, then `Core::sync_remove(id, trash)` with the question's Trash
    /// option (`SyncRemoval::trash`); an alert says what failed (`sync_remove_failed`) or what the Trash refused.
    Remove,
    /// The Add Folder dialog (`HostWindow::AddFolder`).
    AddFolder,
    /// Opens the system's list where on-demand folders are allowed in the file manager (the Mac: System Settings ›
    /// File Providers). Only on a host with `HostFeature::OnDemandFolders`.
    AllowOnDemand,
    /// Gives a member another role: `Core::sync_set_role` (`sync_set_role_failed`).
    SetRole { device_id: String, role: SyncRole },
    /// Removes a member, or a device gone from the folder, for everyone: `Core::sync_remove_device`
    /// (`sync_remove_device_failed`).
    RemoveDevice { device_id: String },
    /// Shares the folder again from this computer: `Core::sync_reshare` (`sync_reshare_failed`), then the new folder's
    /// page and its Share dialog.
    Reshare,
    /// Shows a conflict copy in the file manager.
    RevealConflict { path: String },
    /// Keeps a version of a conflict: `Core::sync_resolve_conflict(id, copy, keep)` (`sync_resolve_failed`).
    Resolve { copy: String, keep: SyncKeep },
}

/// A question the host asks before it runs an item's command: the title, what it does, and Cancel and the confirming
/// button (the default one), drawn in the warning colour when `destructive`.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncQuestion {
    pub title: String,
    pub text: String,
    pub cancel_title: String,
    pub confirm_title: String,
    pub destructive: bool,
}

/// A button of the page.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncPageButton {
    /// Its title; for one drawn as a symbol only, its tooltip.
    pub title: String,
    pub help: String,
    /// What a screen reader says when it is not the title ("Show Photos in Finder").
    pub accessibility_label: Option<String>,
    pub command: SyncPageCommand,
    /// Asked first: the command runs on its confirming button.
    pub question: Option<SyncQuestion>,
}

/// One line of a menu of the page.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum SyncPageMenuEntry {
    Item {
        title: String,
        command: SyncPageCommand,
        /// Drawn with a check mark (the role in force).
        checked: bool,
        enabled: bool,
        /// Drawn in the warning colour.
        destructive: bool,
        question: Option<SyncQuestion>,
    },
    Separator,
}

/// A pop-up menu: its title (what is in force, or what it is for), its tooltip, its entries.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncPageMenu {
    pub title: String,
    pub help: String,
    pub entries: Vec<SyncPageMenuEntry>,
}

/// A folder in the list on the left: its status image, its name (cut in the middle where it does not fit) and grey
/// line, its context menu.
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct SyncPageFolderRow {
    pub id: String,
    pub title: String,
    pub subtitle: String,
    pub badge: SyncBadge,
    /// "Photos, Up to date".
    pub accessibility_label: String,
    pub selected: bool,
    /// Show in Finder, Share…, Open Chat, Edit…, a separator, Stop Syncing….
    pub menu: Vec<SyncPageMenuEntry>,
}

/// How a member is reached, for its image: this computer, the local network, the Internet, a relay, or not connected.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum SyncMemberReach {
    ThisDevice,
    LocalNetwork,
    Internet,
    Relay,
    Away,
}

/// A member of the folder.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncPageMember {
    pub device_id: String,
    /// "Anna (Mac mini)", "Kristian (MacBook) (this Mac)".
    pub name: String,
    /// "Local network · Seedwell 0.1.6", "Offline · until 1 Nov"; empty: none.
    pub line: String,
    pub reach: SyncMemberReach,
    /// "↓ 12 MB   ↑ 3 MB" while it is connected, and its tooltip.
    pub transfer: Option<String>,
    pub transfer_help: String,
    /// Its role ("Read & write"): plain text, or the title of `role_menu`.
    pub role: String,
    /// An owner's menu for another member: the roles (the one in force checked), then Remove from Folder….
    pub role_menu: Option<SyncPageMenu>,
}

/// A conflict copy.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncPageConflict {
    /// The copy, absolute (what the host reveals).
    pub path: String,
    /// The file it is a copy of ("Docs/Report.txt").
    pub title: String,
    /// "MacBook’s version · 2026-10-25 15.32".
    pub line: String,
    pub reveal: SyncPageButton,
    /// "Keep": Current Version, MacBook’s Version, Both.
    pub keep: SyncPageMenu,
}

/// A path the folder cannot sync.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncPageIssue {
    /// The path (line breaks shown as ␍); `None`: the issue is the whole folder's.
    pub path: Option<String>,
    pub reason: String,
}

/// A section of the page: its title, the quiet line it shows while it has no rows, its rows.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncPageMembers {
    pub title: String,
    pub empty: Option<String>,
    pub rows: Vec<SyncPageMember>,
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncPageConflicts {
    pub title: String,
    pub empty: Option<String>,
    pub rows: Vec<SyncPageConflict>,
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncPageIssues {
    pub title: String,
    pub rows: Vec<SyncPageIssue>,
}

/// A device gone from the folder (for its owner): the line and its Remove from Folder… (asks first).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncPageGone {
    pub device_id: String,
    pub line: String,
    pub button: SyncPageButton,
}

/// Every owner and admin of the folder is gone: why, and Share Again from This Mac… (asks first).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncPageReshare {
    pub note: String,
    pub button: SyncPageButton,
}

/// A line of the header that needs the user, with the button that helps (on-demand folders kept out of the file
/// manager).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncPageNotice {
    pub text: String,
    pub button: Option<SyncPageButton>,
}

/// The selected folder's page, top to bottom.
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct SyncFolderPage {
    pub folder_id: String,
    /// The folder's name (cut in the middle where it does not fit).
    pub title: String,
    /// Right of the title, in order: Chat, Share…, Edit…, Stop Syncing….
    pub buttons: Vec<SyncPageButton>,
    /// Where it is ("~/Seedwell/Photos", or the file manager's sidebar for an on-demand folder), and the magnifier
    /// beside it (drawn as a symbol).
    pub place: String,
    pub reveal: SyncPageButton,
    /// The status image and line: "Up to date · 2 devices · Owner · 12 items · 1.2 GB · synced 3 min ago".
    pub badge: SyncBadge,
    pub status: String,
    /// Why it cannot sync (drawn in the error colour).
    pub error: Option<String>,
    /// A quiet note under the status (the folder or its disk is missing).
    pub note: Option<String>,
    /// An on-demand folder the system keeps out of the file manager (only with `HostFeature::OnDemandFolders`).
    pub on_demand_off: Option<SyncPageNotice>,
    /// The box under the header: devices gone, then the Share Again prompt. Drawn while either is there.
    pub gone: Vec<SyncPageGone>,
    pub reshare: Option<SyncPageReshare>,
    /// The Stop Syncing question under the header while it is open: its question, the Trash option, Cancel
    /// ([`SyncPageCommand::CancelRemove`]) and Remove ([`SyncPageCommand::Remove`]).
    pub removal: Option<SyncMenuConfirm>,
    pub members: SyncPageMembers,
    /// Shown while the folder has conflict copies (counted, or listed).
    pub conflicts: Option<SyncPageConflicts>,
    /// Shown while some paths cannot be synced.
    pub issues: Option<SyncPageIssues>,
}

/// No folders yet: the page's one line and its button.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncPageEmpty {
    /// "Keep folders the same on your devices", or why sync does not run.
    pub text: String,
    /// Add Folder… (`None` without the dialog).
    pub add: Option<SyncMenuItem>,
}

/// The whole page.
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct SyncPageModel {
    /// No folders: only this.
    pub empty: Option<SyncPageEmpty>,
    /// The folders, in the menu's order.
    pub folders: Vec<SyncPageFolderRow>,
    /// "Add Folder…" under the list (`None` without the dialog).
    pub add: Option<SyncMenuItem>,
    /// The folder shown: the one asked for, else (none, or gone) the first. The host keeps it as its selection.
    pub selected: Option<String>,
    pub page: Option<SyncFolderPage>,
    /// How often the host reads the selected folder's members (`Core::sync_members`) and its conflict copies
    /// (`Core::sync_conflicts`) again while the page is shown, besides when the selection changes and after a change.
    pub members_refresh_ms: u32,
    pub conflicts_refresh_ms: u32,
    /// Changes whenever anything above does: the host may skip redrawing a page it has drawn.
    pub signature: String,
}

/// What Reveal does with a folder ([`sync_reveal`]).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum SyncReveal {
    /// Shows `path` in the file manager, selected in the folder it is in.
    Show { path: String },
    /// Nothing to reveal (the folder or its disk is not there, or an on-demand folder the file manager does not have):
    /// the folder's page instead, where it says why.
    Page,
}

/// What the page is built from: the host's reads.
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct SyncPageInput {
    pub snapshot: SyncSnapshot,
    /// The folder the user picked.
    pub selected: Option<String>,
    /// The selected folder's members and conflict copies as last read (`Core::sync_members`, `Core::sync_conflicts`,
    /// off the main thread; read again when the selection changes and after a change of roles or members).
    pub members: Vec<SyncMember>,
    pub conflicts: Vec<SyncConflict>,
    /// The channels on this computer (`Core::chat_channels`): a folder's Chat needs its channel here.
    pub channels: Vec<SyncChannel>,
    /// The selected folder is on demand and the system keeps it out of the file manager (the Mac: its File Provider
    /// domain is switched off).
    pub on_demand_off: bool,
    pub now_unix_s: u64,
}

// MARK: Building it

fn button(title: impl Into<String>, help: impl Into<String>, command: SyncPageCommand) -> SyncPageButton {
    SyncPageButton { title: title.into(), help: help.into(), accessibility_label: None, command, question: None }
}

fn item(title: impl Into<String>, command: SyncPageCommand) -> SyncPageMenuEntry {
    SyncPageMenuEntry::Item { title: title.into(), command, checked: false, enabled: true, destructive: false, question: None }
}

fn question(title: String, text: String, confirm: &str, destructive: bool) -> SyncQuestion {
    SyncQuestion { title, text, cancel_title: CANCEL.into(), confirm_title: confirm.into(), destructive }
}

/// The folder's channel, when it is on this computer.
fn chat<'a>(f: &SyncFolder, channels: &'a [SyncChannel]) -> Option<&'a SyncChannel> {
    f.channel_id.as_ref().and_then(|c| channels.iter().find(|x| &x.id == c))
}

const SHARE_HELP: &str = concat!("Make a link that lets another ", computer!(), " join this folder");
const EDIT_HELP: &str = "Rename or move the folder, options";
const STOP: &str = "Stop Syncing…";

/// A folder's context menu in the list.
fn folder_menu(f: &SyncFolder, channels: &[SyncChannel], host: &[HostWindow]) -> Vec<SyncPageMenuEntry> {
    let mut out = vec![item(REVEAL, SyncPageCommand::Reveal)];
    if sync_can_share(f.role) && has(host, Some(HostWindow::FolderShare)) {
        out.push(item("Share…", SyncPageCommand::Share));
    }
    if let Some(c) = chat(f, channels) {
        out.push(item("Open Chat", SyncPageCommand::OpenChat { channel_id: c.id.clone() }));
    }
    if has(host, Some(HostWindow::FolderEditor)) {
        out.push(item("Edit…", SyncPageCommand::Edit));
    }
    out.push(SyncPageMenuEntry::Separator);
    out.push(item(STOP, SyncPageCommand::AskRemove));
    out
}

/// The header's buttons: Chat, Share…, Edit…, Stop Syncing….
fn header_buttons(f: &SyncFolder, channels: &[SyncChannel], host: &[HostWindow]) -> Vec<SyncPageButton> {
    let mut out = Vec::new();
    if let Some(c) = chat(f, channels) {
        out.push(button("Chat", format!("Open “{}”, this folder’s channel", c.name), SyncPageCommand::OpenChat { channel_id: c.id.clone() }));
    }
    if sync_can_share(f.role) && has(host, Some(HostWindow::FolderShare)) {
        out.push(button("Share…", SHARE_HELP, SyncPageCommand::Share));
    }
    if has(host, Some(HostWindow::FolderEditor)) {
        out.push(button("Edit…", EDIT_HELP, SyncPageCommand::Edit));
    }
    out.push(button(STOP, concat!("Stop syncing this folder on this ", computer!(), "; other devices keep their copy"), SyncPageCommand::AskRemove));
    out
}

/// "Up to date · 2 devices · Owner · shared until 1 Nov · 12 items · 1.2 GB · synced 3 min ago".
fn status_line(f: &SyncFolder, now_s: u64, clock: &dyn LocalClock) -> String {
    let mut parts = vec![subtitle(f), role(f.role).to_string()];
    if let Some(until) = f.share_until_s {
        parts.push(format!("shared until {}", local_day(clock, until, now_s)));
    }
    if f.files > 0 {
        parts.push(format!("{} · {}", d::count(f.files, "item"), d::bytes(f.bytes)));
    }
    if f.state == SyncState::UpToDate
        && let Some(t) = f.last_synced_s
    {
        parts.push(format!("synced {}", d::ago(t, now_s, false)));
    }
    parts.join(" · ")
}

/// "Remove Mac mini from “Photos”?" and what it does (a member's or a gone device's removal).
fn remove_device_question(device: &str, folder: &str) -> SyncQuestion {
    let who = if device.trim().is_empty() { "A device" } else { device.trim() };
    question(
        format!("Remove {who} from “{folder}”?"),
        "It stops syncing at once, on every device. It keeps the files it already has.".into(),
        "Remove",
        true,
    )
}

fn reach(m: &SyncMember, device: Option<&SyncDevice>) -> SyncMemberReach {
    if m.is_me {
        return SyncMemberReach::ThisDevice;
    }
    match device.map(|d| d.via.as_str()) {
        Some("LAN") => SyncMemberReach::LocalNetwork,
        Some("Relay") => SyncMemberReach::Relay,
        Some(_) => SyncMemberReach::Internet,
        None => SyncMemberReach::Away,
    }
}

/// The roles an owner hands out on the page, in the menu's order.
const ROLES: [SyncRole; 4] = [SyncRole::Owner, SyncRole::Admin, SyncRole::ReadWrite, SyncRole::ReadOnly];

fn member(f: &SyncFolder, m: &SyncMember, device: Option<&SyncDevice>, now_s: u64, clock: &dyn LocalClock) -> SyncPageMember {
    let name = if m.is_me {
        let mine = if m.name.is_empty() { concat!("This ", computer!()) } else { m.name.as_str() };
        format!(concat!("{} (this ", computer!(), ")"), named(mine, &m.user))
    } else {
        named(&m.name, &m.user)
    };
    let expires_day = m.expires_s.map(|t| local_day(clock, t, now_s));
    let role_menu = (f.role == SyncRole::Owner && !m.is_me).then(|| {
        let mut entries: Vec<SyncPageMenuEntry> = ROLES
            .iter()
            .map(|&r| SyncPageMenuEntry::Item {
                title: role(r).into(),
                command: SyncPageCommand::SetRole { device_id: m.device_id.clone(), role: r },
                checked: r == m.role,
                enabled: r != m.role,
                destructive: false,
                question: None,
            })
            .collect();
        entries.push(SyncPageMenuEntry::Separator);
        entries.push(SyncPageMenuEntry::Item {
            title: "Remove from Folder…".into(),
            command: SyncPageCommand::RemoveDevice { device_id: m.device_id.clone() },
            checked: false,
            enabled: true,
            destructive: false,
            question: Some(remove_device_question(&m.name, &f.name)),
        });
        SyncPageMenu { title: role(m.role).into(), help: format!("Change what {} may do in this folder, or remove it", m.name), entries }
    });
    SyncPageMember {
        device_id: m.device_id.clone(),
        name,
        line: sync_member_line(m.clone(), device.cloned(), expires_day),
        reach: reach(m, device),
        transfer: device.map(|d| format!("↓ {}   ↑ {}", d::bytes(d.rx_bytes), d::bytes(d.tx_bytes))),
        transfer_help: "Received from and sent to this device since it connected".into(),
        role: role(m.role).into(),
        role_menu,
    }
}

fn conflict(c: &SyncConflict) -> SyncPageConflict {
    let resolve = |keep| SyncPageCommand::Resolve { copy: c.copy.clone(), keep };
    let theirs = if c.device.is_empty() { "The Copy".to_string() } else { format!("{}’s Version", c.device) };
    SyncPageConflict {
        path: c.path.clone(),
        title: c.original.clone(),
        line: format!("{}’s version · {}", c.device, c.when),
        reveal: button(REVEAL, "Reveal the conflict copy next to the file", SyncPageCommand::RevealConflict { path: c.path.clone() }),
        keep: SyncPageMenu {
            title: "Keep".into(),
            help: concat!("Keep one version (the other goes to the folder’s archive on this ", computer!(), "), or both as separate files").into(),
            entries: vec![item("Current Version", resolve(SyncKeep::Original)), item(theirs, resolve(SyncKeep::Copy)), item("Both", resolve(SyncKeep::Both))],
        },
    }
}

fn page(i: &SyncPageInput, f: &SyncFolder, removal: Option<SyncMenuConfirm>, clock: &dyn LocalClock, host: &[HostWindow], on_demand: bool) -> SyncFolderPage {
    let now_s = i.now_unix_s;
    let devices: Vec<&SyncDevice> = i.snapshot.devices.iter().filter(|d| d.folders.contains(&f.id)).collect();
    let members = sync_members_ordered(i.members.clone())
        .iter()
        .map(|m| member(f, m, devices.iter().copied().find(|d| d.id == m.device_id), now_s, clock))
        .collect::<Vec<_>>();
    let members = SyncPageMembers {
        title: "Members".into(),
        empty: members.is_empty().then(|| if f.state == SyncState::WaitingForAdmin { join_note(f) } else { "No members known yet".into() }),
        rows: members,
    };
    let conflicts = (f.conflicts > 0 || !i.conflicts.is_empty()).then(|| SyncPageConflicts {
        title: "Conflicts".into(),
        // (`count` would say "copys".)
        empty: i.conflicts.is_empty().then(|| if f.conflicts == 1 { "1 conflict copy".into() } else { format!("{} conflict copies", f.conflicts) }),
        rows: i.conflicts.iter().map(conflict).collect(),
    });
    let issues = (!f.issues.is_empty()).then(|| SyncPageIssues {
        title: "Issues".into(),
        rows: f
            .issues
            .iter()
            .map(|x| SyncPageIssue { path: (!x.path.is_empty()).then(|| x.path.replace('\r', "␍")), reason: x.reason.clone() })
            .collect(),
    });
    let gone = f
        .gone
        .iter()
        .map(|g| {
            let p = sync_gone_prompt(g.clone(), f.name.clone(), false, now_s);
            SyncPageGone {
                device_id: g.device_id.clone(),
                line: p.line,
                button: SyncPageButton {
                    accessibility_label: Some(p.help.clone()),
                    question: Some(question(p.title, p.text, &p.confirm, true)),
                    ..button(p.button, p.help, SyncPageCommand::RemoveDevice { device_id: g.device_id.clone() })
                },
            }
        })
        .collect();
    let reshare = f.can_reshare.then(|| {
        let p = sync_reshare_prompt(f.clone());
        SyncPageReshare {
            note: p.note,
            button: SyncPageButton { question: Some(question(p.title, p.text, &p.confirm, false)), ..button(p.button, p.help, SyncPageCommand::Reshare) },
        }
    });
    let on_demand_off = (on_demand && f.on_demand && i.on_demand_off).then(|| {
        let n = sync_on_demand_notice(false);
        SyncPageNotice { text: sync_on_demand_off_line(), button: n.button.map(|b| button(b, n.help, SyncPageCommand::AllowOnDemand)) }
    });
    SyncFolderPage {
        folder_id: f.id.clone(),
        title: f.name.clone(),
        buttons: header_buttons(f, &i.channels, host),
        place: sync_place_line(f.clone(), trs_platform::dirs::shown(Path::new(&f.path))),
        reveal: SyncPageButton { accessibility_label: Some(format!(concat!("Show {} in ", file_manager!()), f.name)), ..button(REVEAL, REVEAL, SyncPageCommand::Reveal) },
        badge: badge(f),
        status: status_line(f, now_s, clock),
        error: f.error.clone().filter(|_| f.state == SyncState::Error),
        note: (f.state == SyncState::Missing)
            .then(|| "The folder or its disk is not there. Nothing is deleted on other devices; syncing starts again when it is back.".into()),
        on_demand_off,
        gone,
        reshare,
        removal,
        members,
        conflicts,
        issues,
    }
}

/// The question under the header, for the folder it is open for.
fn removal_strip(f: &SyncFolder, r: &Arc<SyncRemoval>) -> Option<SyncMenuConfirm> {
    let (open, only_here, trash) = r.state_now();
    (open.as_deref() == Some(f.id.as_str())).then(|| SyncMenuConfirm {
        folder_id: f.id.clone(),
        question: sync_remove_folder_question(f.clone(), only_here),
        trash_label: sync_remove_trash_label(),
        trash,
        cancel_title: CANCEL.into(),
        cancel_help: "Keep syncing this folder".into(),
        remove_title: "Remove".into(),
        remove_help: sync_remove_help(trash),
    })
}

/// The Folder Sync page for `input`, with the Stop Syncing question `removal` has open (the page's own
/// [`SyncRemoval`]: it closes when another folder is selected or its folder goes). `host`: the windows the host has
/// (`host_windows_all()` on the Mac): Share… needs `FolderShare`, Edit… `FolderEditor`, Add Folder… `AddFolder`.
/// `features`: what else it has (`host_features_all()` on the Mac): the notice of an on-demand folder kept out of the
/// file manager needs `OnDemandFolders`.
#[uniffi::export]
pub fn sync_page_model(
    input: SyncPageInput,
    removal: Arc<SyncRemoval>,
    clock: Arc<dyn LocalClock>,
    host: Vec<HostWindow>,
    features: Vec<HostFeature>,
) -> SyncPageModel {
    let on_demand = features.contains(&HostFeature::OnDemandFolders);
    let s = &input.snapshot;
    let selected = input.selected.as_ref().filter(|id| s.folders.iter().any(|f| &&f.id == id)).or_else(|| s.folders.first().map(|f| &f.id)).cloned();
    // The question goes with its folder: another one selected, or the folder removed.
    removal.keep_only(&selected.iter().map(String::as_str).collect());
    let add = has(&host, Some(HostWindow::AddFolder)).then(add_folder_item);
    let empty = s.folders.is_empty().then(|| SyncPageEmpty {
        text: if s.running || s.problem.is_none() { "Keep folders the same on your devices".into() } else { s.problem.clone().unwrap_or_default() },
        add: add.clone(),
    });
    let folders = s
        .folders
        .iter()
        .map(|f| {
            let sub = subtitle(f);
            SyncPageFolderRow {
                id: f.id.clone(),
                title: f.name.clone(),
                accessibility_label: format!("{}, {sub}", f.name),
                subtitle: sub,
                badge: badge(f),
                selected: selected.as_ref() == Some(&f.id),
                menu: folder_menu(f, &input.channels, &host),
            }
        })
        .collect();
    let page = selected
        .as_ref()
        .and_then(|id| s.folders.iter().find(|f| &f.id == id))
        .map(|f| page(&input, f, removal_strip(f, &removal), clock.as_ref(), &host, on_demand));
    let mut m = SyncPageModel {
        empty,
        folders,
        add,
        selected,
        page,
        members_refresh_ms: MEMBERS_REFRESH_MS,
        conflicts_refresh_ms: CONFLICTS_REFRESH_MS,
        signature: String::new(),
    };
    let mut h = std::collections::hash_map::DefaultHasher::new();
    format!("{m:?}").hash(&mut h);
    m.signature = format!("{:016x}", h.finish());
    m
}

/// What Reveal does with folder `folder` (the page's, a list row's or the menu's): show its place in the file manager,
/// or its page when there is nothing to show. `on_demand_place`: for an on-demand folder, where the file manager has
/// it (the Mac: its File Provider domain's root), `None` when it has none.
#[uniffi::export]
pub fn sync_reveal(folder: SyncFolder, on_demand_place: Option<String>) -> SyncReveal {
    let path = if folder.on_demand { on_demand_place } else { Path::new(&folder.path).exists().then_some(folder.path) };
    path.map_or(SyncReveal::Page, |path| SyncReveal::Show { path })
}

impl SyncRemoval {
    /// Closes the question unless it is about one of `ids`.
    pub(crate) fn keep_only(&self, ids: &HashSet<&str>) {
        self.with(|r| r.keep_only(ids));
    }
}

#[cfg(test)]
#[path = "sync_page_tests.rs"]
mod tests;
