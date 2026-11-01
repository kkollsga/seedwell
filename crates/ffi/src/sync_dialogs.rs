//! The Add Folder and Edit dialogs of Folder Sync (SPEC 7G.5) as models every host draws (docs/CROSS-PLATFORM.md §1):
//! every title, label, placeholder, note and tooltip, which choices the Location popup offers, what the folder pickers
//! say, when Start and Save may be pressed and what they run. The host keeps the form as a value, draws it, shows the
//! folder pickers and runs Start or Save off the main thread.
//!
//! - Add Folder: [`Core::sync_add_form`], [`Core::sync_add_set_link`], [`sync_add_set_location`],
//!   `sync_add_choose_parent`, [`Core::sync_add_window`], [`Core::sync_add_start`].
//! - Edit: [`Core::sync_edit_form`], [`sync_edit_choose_parent`], [`sync_edit_window`], [`Core::sync_edit_save`].

use std::path::Path;

use trs_platform::{computer, dirs};

use crate::devices_text::FollowLine;
use crate::sync::{SyncFolder, SyncState};
use crate::sync_text::{
    key_usable, link_line, role, subtitle, suggested_name, sync_add_can_start, sync_add_set_key, sync_can_share, sync_edit_share_note,
    sync_existing_folder, sync_folder_name_problem, sync_location_note, sync_on_demand_help, sync_on_demand_label, sync_on_demand_note,
    sync_place_line, trim_spaces, SyncAddForm, SyncAddMode, SyncOnDemandChange,
};
use crate::types::CoreError;
use crate::Core;

const CAPTION: &str = "Folder Sync";
const CANCEL: &str = "Cancel";
const CANCEL_HELP: &str = "Close without changing anything";
const LABEL_LOCATION: &str = "Location";

/// A path as the host shows one ("~/Seedwell/Photos").
fn shown(path: &str) -> String {
    dirs::shown(Path::new(path))
}

/// What a folder picker says: its button, its message, and the folder it opens in (`None`: the system's choice).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncFolderPicker {
    pub prompt: String,
    pub message: String,
    pub start: Option<String>,
}

/// One side of the Add Folder dialog's switch.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncAddModeChoice {
    pub mode: SyncAddMode,
    /// "From this Mac", "Shared with me".
    pub title: String,
}

/// A place in the Location popup: the path (the value the form keeps) and how it reads.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncLocationChoice {
    pub path: String,
    pub title: String,
}

/// The Add Folder dialog as drawn from a [`SyncAddForm`].
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncAddWindowModel {
    /// The small caption over the title: "Folder Sync".
    pub caption: String,
    /// "Add Folder".
    pub title: String,
    /// The switch between the sides, in order.
    pub modes: Vec<SyncAddModeChoice>,
    // From this computer
    pub folder_label: String,
    /// The folder chosen ("~/Novel"), or "No folder chosen" (drawn quieter: `folder_chosen` false).
    pub folder_text: String,
    pub folder_chosen: bool,
    /// "Choose…" and its tooltip: the folder picker.
    pub choose_title: String,
    pub choose_help: String,
    pub folder_picker: SyncFolderPicker,
    /// "You become its owner. After Start, Share… makes a link for each Mac that should join."
    pub local_note: String,
    // Shared with me
    pub link_label: String,
    pub link_placeholder: String,
    /// What the pasted link is ("Read & write · Photos") or why it cannot be used; empty while nothing is pasted.
    pub link_line: String,
    /// That line is about a link that can be joined (drawn with a check mark; else as a warning).
    pub link_ok: bool,
    pub location_label: String,
    /// The popup's places: the link's default place, the folder of that name already there, the one the user chose.
    /// After them a separator and "Choose…" (the location picker). Empty: only `location_placeholder`.
    pub locations: Vec<SyncLocationChoice>,
    /// The popup's value now (one of `locations`' paths, or empty).
    pub location: String,
    pub location_placeholder: String,
    pub location_choose: String,
    /// The popup can be used (a usable link is pasted).
    pub location_enabled: bool,
    pub location_help: String,
    /// Under the popup: a folder of that name is there already, and what syncing into it does.
    pub location_note: Option<String>,
    pub location_picker: SyncFolderPicker,
    // Both sides
    /// "Files on demand", its tooltip, and what it means (while it is on).
    pub on_demand_label: String,
    pub on_demand_help: String,
    pub on_demand_note: Option<String>,
    /// "On all my devices" (a folder of this computer, while this computer is paired); `on` is the form's choice.
    pub follow: Option<FollowLine>,
    pub cancel_title: String,
    pub cancel_help: String,
    /// "Start", its tooltip, and whether it may be pressed.
    pub confirm_title: String,
    pub confirm_help: String,
    pub can_start: bool,
    /// After Start: the new folder's Share dialog opens (a folder of this computer: sharing it is the next step).
    pub share_after_start: bool,
}

/// The user picked a place in the Location popup: it stays when another link is pasted, unless it is the link's
/// default place.
#[uniffi::export]
pub fn sync_add_set_location(form: SyncAddForm, location: String) -> SyncAddForm {
    let chosen = form.default_location.as_deref() != Some(location.as_str());
    SyncAddForm { location, location_chosen: chosen, ..form }
}

fn locations(form: &SyncAddForm) -> Vec<SyncLocationChoice> {
    let mut out: Vec<String> = form.default_location.iter().chain(form.existing_folder.iter()).cloned().collect();
    if !form.location.is_empty() && !out.contains(&form.location) {
        out.push(form.location.clone());
    }
    out.into_iter().map(|path| SyncLocationChoice { title: shown(&path), path }).collect()
}

/// Where the location picker opens: the folder the location is in, when it is there.
fn location_start(form: &SyncAddForm) -> Option<String> {
    let parent = Path::new(&form.location).parent()?;
    (!parent.as_os_str().is_empty() && parent.is_dir()).then(|| parent.display().to_string())
}

fn add_window(form: &SyncAddForm, local_on_demand_note: String) -> SyncAddWindowModel {
    let local = form.mode == SyncAddMode::Local;
    let usable = key_usable(form);
    SyncAddWindowModel {
        caption: CAPTION.into(),
        title: "Add Folder".into(),
        modes: crate::sync_text::sync_add_modes()
            .into_iter()
            .map(|mode| SyncAddModeChoice { mode, title: crate::sync_text::sync_add_mode_label(mode) })
            .collect(),
        folder_label: "Folder".into(),
        folder_text: if form.folder.is_empty() { "No folder chosen".into() } else { shown(&form.folder) },
        folder_chosen: !form.folder.is_empty(),
        choose_title: "Choose…".into(),
        choose_help: "Pick the folder to sync".into(),
        folder_picker: SyncFolderPicker { prompt: "Choose".into(), message: "Choose a folder to keep the same on your devices".into(), start: None },
        local_note: concat!("You become its owner. After Start, Share… makes a link for each ", computer!(), " that should join.").into(),
        link_label: "Link".into(),
        link_placeholder: "seedwell://sync/…".into(),
        link_line: link_line(&form.key, form.info.as_ref(), form.problem.as_deref()),
        link_ok: usable,
        location_label: LABEL_LOCATION.into(),
        locations: locations(form),
        location: form.location.clone(),
        location_placeholder: "Paste a link first".into(),
        location_choose: "Choose…".into(),
        location_enabled: usable,
        location_help: concat!("Where the shared folder goes on this ", computer!()).into(),
        location_note: if usable {
            sync_location_note(form.location.clone(), form.existing_folder.clone(), form.existing_folder.as_deref().map(shown).unwrap_or_default())
        } else {
            None
        },
        location_picker: SyncFolderPicker {
            prompt: "Choose".into(),
            message: "Choose where the shared folder goes".into(),
            start: location_start(form),
        },
        on_demand_label: sync_on_demand_label(),
        on_demand_help: sync_on_demand_help(),
        on_demand_note: form.on_demand.then(|| if local { local_on_demand_note } else { sync_on_demand_note(SyncOnDemandChange::Join, String::new()) }),
        follow: (local && form.follow.shown).then(|| FollowLine { on: form.all_devices, ..form.follow.clone() }),
        cancel_title: CANCEL.into(),
        cancel_help: CANCEL_HELP.into(),
        confirm_title: "Start".into(),
        confirm_help: if local { "Start syncing this folder" } else { "Join the shared folder" }.into(),
        can_start: sync_add_can_start(form.clone()),
        share_after_start: local,
    }
}

// MARK: Edit

/// The Edit dialog's choices for one folder (SPEC 7G.5). The host keeps it as a value, edits `name`, `paused`,
/// `lan_only`, `on_demand`, `all_devices` and `options_open` itself, and the place with [`sync_edit_choose_parent`].
/// Everything applies on Save.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncEditForm {
    pub folder_id: String,
    /// The folder as it was read (`None`: it is no longer synced here).
    pub folder: Option<SyncFolder>,
    pub name: String,
    /// The new parent folder chosen with Move… (the folder keeps its name).
    pub new_parent: Option<String>,
    pub paused: bool,
    pub lan_only: bool,
    pub on_demand: bool,
    /// "On all my devices" as the core has it (hidden while this computer is not paired), and the choice.
    pub follow: FollowLine,
    pub all_devices: bool,
    /// The Options disclosure is open.
    pub options_open: bool,
}

/// The Edit dialog as drawn from a [`SyncEditForm`].
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncEditWindowModel {
    pub caption: String,
    /// The folder's name ("Folder" once it is gone), and "Owner · Up to date" under it.
    pub title: String,
    pub subtitle: Option<String>,
    /// "This folder is no longer synced on this Mac." instead of the form.
    pub gone: Option<String>,
    pub name_label: String,
    pub name_help: String,
    pub location_label: String,
    /// Where it goes ("~/Documents"), drawn quieter until Move… picked a new place; the full path as its tooltip.
    pub location: String,
    pub location_moved: bool,
    pub location_help: String,
    /// "Move…": the folder picker (not for an on-demand folder: the file manager shows it where it is).
    pub move_title: String,
    pub move_help: String,
    pub move_enabled: bool,
    pub move_picker: SyncFolderPicker,
    pub on_demand_label: String,
    pub on_demand_help: String,
    /// What Files on demand means as it is set now (`None` for a full folder that stays full).
    pub on_demand_note: Option<String>,
    /// "On all my devices" while it is shown; `on` is the form's choice.
    pub follow: Option<FollowLine>,
    /// The Share section: its caption, what this computer's role lets it do, and Share… (its Share dialog) when it
    /// may share.
    pub share_caption: String,
    pub share_note: String,
    pub share_title: Option<String>,
    pub share_help: String,
    /// The Options disclosure and what is in it.
    pub options_title: String,
    pub pause_label: String,
    pub pause_help: String,
    pub lan_label: String,
    pub lan_help: String,
    /// "Rescan Now" (`Core::sync_rescan`), its tooltip and the note beside it.
    pub rescan_title: String,
    pub rescan_help: String,
    pub rescan_note: String,
    pub cancel_title: String,
    pub cancel_help: String,
    /// "Save": enabled while the folder is there; with nothing changed it only closes the dialog.
    pub confirm_title: String,
    pub confirm_help: String,
    pub can_save: bool,
    pub changed: bool,
}

/// The folder's parent on disk (where Move… starts), or the new one Move… chose.
fn parent(form: &SyncEditForm) -> String {
    form.new_parent.clone().unwrap_or_else(|| {
        form.folder.as_ref().and_then(|f| Path::new(&f.path).parent()).map(|p| p.display().to_string()).unwrap_or_default()
    })
}

/// Where the folder ends up: the parent and the name as typed.
fn target(form: &SyncEditForm) -> String {
    Path::new(&parent(form)).join(trim_spaces(&form.name)).display().to_string()
}

/// The folder goes somewhere else: compared as paths, so `/` and `\` on Windows (or a trailing separator) are the same.
fn moves(form: &SyncEditForm, f: &SyncFolder) -> bool {
    Path::new(&target(form)) != Path::new(&f.path)
}

fn changed(form: &SyncEditForm) -> bool {
    form.folder.as_ref().is_some_and(|f| {
        moves(form, f)
            || form.paused != (f.state == SyncState::Paused)
            || form.lan_only != f.lan_only
            || form.on_demand != f.on_demand
            || form.all_devices != form.follow.on
    })
}

/// Move… chose `parent`: the folder goes below it, with its name.
#[uniffi::export]
pub fn sync_edit_choose_parent(form: SyncEditForm, parent: String) -> SyncEditForm {
    SyncEditForm { new_parent: Some(parent), ..form }
}

/// The Edit dialog drawn from `form`. `share`: the host has the folder's Share dialog (`HostWindow::FolderShare`).
#[uniffi::export]
pub fn sync_edit_window(form: SyncEditForm, share: bool) -> SyncEditWindowModel {
    let f = form.folder.as_ref();
    let on_demand_note = f.and_then(|f| {
        let change = match (form.on_demand, f.on_demand) {
            (true, true) => Some(SyncOnDemandChange::Stays),
            (true, false) => Some(SyncOnDemandChange::TurnOn),
            (false, true) => Some(SyncOnDemandChange::TurnOff),
            (false, false) => None,
        };
        change.map(|c| sync_on_demand_note(c, shown(&f.path)))
    });
    let parent = parent(&form);
    SyncEditWindowModel {
        caption: CAPTION.into(),
        title: f.map_or_else(|| "Folder".into(), |f| f.name.clone()),
        subtitle: f.map(|f| format!("{} · {}", role(f.role), subtitle(f))),
        gone: f.is_none().then(|| concat!("This folder is no longer synced on this ", computer!(), ".").into()),
        name_label: "Name".into(),
        name_help: concat!("Renames the folder on this ", computer!(), " only. Other devices keep their own name.").into(),
        location_label: LABEL_LOCATION.into(),
        location: f.map(|f| sync_place_line(f.clone(), shown(&parent))).unwrap_or_default(),
        location_moved: form.new_parent.is_some(),
        location_help: target(&form),
        move_title: "Move…".into(),
        move_help: concat!("Choose another folder for it on this ", computer!(), "; it keeps its name").into(),
        move_enabled: f.is_some_and(|f| !f.on_demand),
        move_picker: SyncFolderPicker {
            prompt: "Move Here".into(),
            message: format!("Choose where “{}” goes. It keeps its name.", form.name),
            start: Path::new(&parent).is_dir().then(|| parent.clone()),
        },
        on_demand_label: sync_on_demand_label(),
        on_demand_help: sync_on_demand_help(),
        on_demand_note,
        follow: form.follow.shown.then(|| FollowLine { on: form.all_devices, ..form.follow.clone() }),
        share_caption: "Share".into(),
        share_note: f.map(|f| sync_edit_share_note(f.role)).unwrap_or_default(),
        share_title: f.filter(|f| share && sync_can_share(f.role)).map(|_| "Share…".into()),
        share_help: concat!("Make a link that lets another ", computer!(), " join this folder").into(),
        options_title: "Options".into(),
        pause_label: "Pause syncing".into(),
        pause_help: "Stop sending and receiving changes for this folder until you resume".into(),
        lan_label: "Only sync on the local network".into(),
        lan_help: concat!("Sync this folder only with devices on the same network as this ", computer!(), ", never over the Internet").into(),
        rescan_title: "Rescan Now".into(),
        rescan_help: "Look at every file of the folder again now".into(),
        rescan_note: "Changes are seen within seconds; a full scan also runs every hour.".into(),
        cancel_title: CANCEL.into(),
        cancel_help: CANCEL_HELP.into(),
        confirm_title: "Save".into(),
        confirm_help: "Apply the new name, place and options".into(),
        can_save: f.is_some(),
        changed: changed(&form),
    }
}

#[uniffi::export]
impl Core {
    /// The Add Folder dialog's form as it opens: From this computer, or Shared with me with `prefill` (a link that
    /// opened the app, or the pasteboard's when `sync_looks_like_link`) read already.
    pub fn sync_add_form(&self, prefill: Option<String>) -> SyncAddForm {
        let form = SyncAddForm { follow: self.sync_follow_line_new(false), ..crate::sync_text::sync_add_form_new() };
        let form = SyncAddForm { all_devices: form.follow.on, ..form };
        match prefill {
            Some(text) => SyncAddForm { mode: SyncAddMode::Shared, ..self.sync_add_set_link(form, text) },
            None => form,
        }
    }

    /// The form with `text` typed or pasted as its link: what it is (or why it cannot be joined), its default place
    /// (the location follows it unless the user chose one) and a folder of that name already there.
    pub fn sync_add_set_link(&self, form: SyncAddForm, text: String) -> SyncAddForm {
        let t = text.trim();
        let info = if t.is_empty() { None } else { self.sync_parse_link(t.to_owned()) };
        let default_location = info.as_ref().and_then(|_| self.sync_join_path(t.to_owned()));
        let existing_folder = info.as_ref().and_then(|i| {
            let synced = self.sync_snapshot().folders.into_iter().map(|f| f.path).collect();
            default_location.clone().and_then(|next| sync_existing_folder(suggested_name(i), next, synced))
        });
        let problem = if info.is_none() && !t.is_empty() { self.sync_link_problem(t.to_owned()) } else { None };
        SyncAddForm { existing_folder, ..sync_add_set_key(form, text, info, default_location, problem) }
    }

    /// The Add Folder dialog drawn from `form` (it reads whether a folder made on demand is on another disk).
    pub fn sync_add_window(&self, form: SyncAddForm) -> SyncAddWindowModel {
        let note = if form.mode == SyncAddMode::Local && form.on_demand { self.sync_on_demand_add_note(form.folder.clone()) } else { String::new() };
        add_window(&form, note)
    }

    /// Start: syncs the chosen folder from this computer (on demand when chosen; on all the user's devices when that
    /// is chosen and shown), or joins the linked one into the location. Slow (the disk, the network): call it off the
    /// main thread. Then the host closes the dialog; for an on-demand folder it adds the folder to the file manager,
    /// and with `share_after_start` it opens the new folder's Share dialog. On failure the dialog stays open and shows
    /// `core_error_message`.
    pub fn sync_add_start(&self, form: SyncAddForm) -> Result<SyncFolder, CoreError> {
        if !sync_add_can_start(form.clone()) {
            let message = match form.mode {
                SyncAddMode::Local => "Choose a folder to sync.".to_owned(),
                SyncAddMode::Shared if form.key.trim().is_empty() => "Paste a link to the folder.".to_owned(),
                SyncAddMode::Shared if key_usable(&form) => "Choose where the folder goes.".to_owned(),
                SyncAddMode::Shared => link_line(&form.key, form.info.as_ref(), form.problem.as_deref()),
            };
            return Err(CoreError::Failed { message });
        }
        let key = form.key.trim().to_owned();
        let f = match (form.mode, form.on_demand) {
            (SyncAddMode::Local, false) => self.sync_add_folder(form.folder.clone())?,
            // Its files go into this computer's storage before it ever opens: added whole or not at all.
            (SyncAddMode::Local, true) => self.sync_add_folder_on_demand(form.folder.clone())?,
            (SyncAddMode::Shared, false) => self.sync_join(key, form.location.clone())?,
            (SyncAddMode::Shared, true) => self.sync_join_on_demand(key, form.location.clone())?,
        };
        // On all my devices: the others join it by themselves (this computer is its owner). Added either way: the Edit
        // dialog shows the toggle as it is.
        if form.mode == SyncAddMode::Local && form.follow.shown && form.all_devices {
            let _ = self.sync_set_follow(f.id.clone(), true);
        }
        Ok(f)
    }

    /// The Edit dialog's form for folder `id` as it is now (read again after a failed Save).
    pub fn sync_edit_form(&self, id: String) -> SyncEditForm {
        let folder = self.sync_snapshot().folders.into_iter().find(|f| f.id == id);
        let follow = self.sync_follow_line(id.clone());
        SyncEditForm {
            name: folder.as_ref().map(|f| f.name.clone()).unwrap_or_default(),
            new_parent: None,
            paused: folder.as_ref().is_some_and(|f| f.state == SyncState::Paused),
            lan_only: folder.as_ref().is_some_and(|f| f.lan_only),
            on_demand: folder.as_ref().is_some_and(|f| f.on_demand),
            all_devices: follow.on,
            follow,
            options_open: false,
            folder,
            folder_id: id,
        }
    }

    /// Save: applies what changed, in an order that works (back to full first, so the place is an ordinary folder
    /// again; then the place and name, pause, local network, on demand, and on all my devices). Slow: call it off the
    /// main thread. Returns the folder as it is now (an on-demand one goes to the file manager); nothing changed: the
    /// folder as it is. On failure the host shows `core_error_message` and reads the form again.
    pub fn sync_edit_save(&self, form: SyncEditForm) -> Result<SyncFolder, CoreError> {
        let Some(f) = form.folder.clone() else {
            return Err(CoreError::Failed { message: concat!("This folder is no longer synced on this ", computer!(), ".").into() });
        };
        if !changed(&form) {
            return Ok(f);
        }
        if let Some(p) = sync_folder_name_problem(form.name.clone()) {
            return Err(CoreError::Failed { message: p });
        }
        let id = form.folder_id.clone();
        let target = target(&form);
        if !form.on_demand && f.on_demand {
            self.sync_set_on_demand(id.clone(), false, None)?;
        }
        if moves(&form, &f) {
            if form.new_parent.is_some() {
                self.sync_move(id.clone(), target)?;
            } else {
                self.sync_rename(id.clone(), trim_spaces(&form.name).to_owned())?;
            }
        }
        if form.paused != (f.state == SyncState::Paused) {
            self.sync_pause(id.clone(), form.paused)?;
        }
        if form.lan_only != f.lan_only {
            self.sync_set_lan_only(id.clone(), form.lan_only)?;
        }
        if form.on_demand && !f.on_demand {
            self.sync_set_on_demand(id.clone(), true, None)?;
        }
        if form.all_devices != form.follow.on {
            self.sync_set_follow(id.clone(), form.all_devices)?;
        }
        Ok(self.sync_snapshot().folders.into_iter().find(|x| x.id == id).unwrap_or(f))
    }
}

#[cfg(test)]
#[path = "sync_dialogs_tests.rs"]
mod tests;
