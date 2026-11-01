use super::*;
use crate::library::tests::mock;
use crate::sync::SyncRole;
use crate::sync_text::sync_add_choose_parent;

/// The mock with its default place for joined folders in `dir` (never the real home folder).
fn core_in(dir: &Path) -> (std::sync::Arc<Core>, tempfile::TempDir) {
    let (core, d) = mock();
    let mut s = core.sync_settings();
    s.default_location = dir.display().to_string();
    core.set_sync_settings(s).unwrap();
    (core, d)
}

#[test]
fn the_add_dialog_opens_on_this_computer_with_its_texts() {
    let places = tempfile::tempdir().unwrap();
    let (core, _d) = core_in(places.path());
    let form = core.sync_add_form(None);
    assert_eq!(form.mode, SyncAddMode::Local);
    let w = core.sync_add_window(form.clone());
    let computer = computer!();
    assert_eq!((w.caption.as_str(), w.title.as_str()), ("Folder Sync", "Add Folder"));
    assert_eq!(w.modes.iter().map(|m| m.title.clone()).collect::<Vec<_>>(), [format!("From this {computer}"), "Shared with me".into()]);
    assert_eq!((w.folder_text.as_str(), w.folder_chosen, w.can_start), ("No folder chosen", false, false));
    assert_eq!((w.choose_title.as_str(), w.folder_picker.prompt.as_str()), ("Choose…", "Choose"));
    assert_eq!(w.local_note, format!("You become its owner. After Start, Share… makes a link for each {computer} that should join."));
    assert_eq!((w.confirm_title.as_str(), w.confirm_help.as_str(), w.share_after_start), ("Start", "Start syncing this folder", true));
    assert_eq!((w.cancel_title.as_str(), w.cancel_help.as_str()), ("Cancel", "Close without changing anything"));
    assert_eq!(w.follow, None, "this computer is not paired");
    assert_eq!(w.on_demand_note, None);
    // A folder chosen: Start.
    let notes = places.path().join("Notes");
    std::fs::create_dir(&notes).unwrap();
    let form = SyncAddForm { folder: notes.display().to_string(), on_demand: true, ..form };
    let w = core.sync_add_window(form.clone());
    assert!(w.can_start && w.folder_chosen);
    assert_eq!(w.on_demand_note, Some(core.sync_on_demand_add_note(form.folder.clone())));
    let f = core.sync_add_start(SyncAddForm { on_demand: false, ..form }).unwrap();
    assert_eq!((f.name.as_str(), f.role), ("Notes", SyncRole::Owner));
    // Nothing chosen: Start says why.
    let e = core.sync_add_start(core.sync_add_form(None)).unwrap_err();
    assert_eq!(crate::sync_text::core_error_message(e), "Choose a folder to sync.");
}

#[test]
fn a_pasted_link_brings_its_place_and_a_folder_of_that_name_already_there() {
    let places = tempfile::tempdir().unwrap();
    let (core, _d) = core_in(places.path());
    let f = core.sync_add_folder("/tmp/seedwell-mock/Team".into()).unwrap();
    let link = core.sync_create_invite(f.id.clone(), SyncRole::ReadWrite, 1, 7, None).unwrap().link;
    core.sync_remove(f.id, false).unwrap();
    let form = core.sync_add_form(Some(link.clone()));
    assert_eq!(form.mode, SyncAddMode::Shared);
    let w = core.sync_add_window(form.clone());
    assert_eq!((w.link_line.as_str(), w.link_ok, w.location_enabled), ("Read & write · Team", true, true));
    let plain = places.path().join("Team").display().to_string();
    assert_eq!((form.default_location.as_deref(), form.existing_folder.as_deref()), (Some(plain.as_str()), None));
    assert_eq!(w.locations.iter().map(|l| l.path.clone()).collect::<Vec<_>>(), std::slice::from_ref(&plain));
    assert_eq!((w.location.as_str(), w.location_note.as_deref()), (plain.as_str(), None));
    assert_eq!((w.confirm_help.as_str(), w.share_after_start), ("Join the shared folder", false));
    assert_eq!(w.location_picker.start.as_deref(), Some(places.path().display().to_string().as_str()));
    // A folder named Team in that place already (the real core then numbers the new one): both are offered.
    std::fs::create_dir(places.path().join("Team")).unwrap();
    let next = places.path().join("Team 2").display().to_string();
    let existing = sync_existing_folder("Team".into(), next.clone(), Vec::new());
    assert_eq!(existing.as_deref(), Some(plain.as_str()));
    let form = SyncAddForm { default_location: Some(next.clone()), location: next.clone(), existing_folder: existing, ..form };
    let w = core.sync_add_window(form.clone());
    assert_eq!(w.locations.iter().map(|l| l.path.clone()).collect::<Vec<_>>(), [next.clone(), plain.clone()]);
    assert!(w.location_note.unwrap().ends_with("Choose it in Location to sync into it instead of a new folder."));
    let existing = plain;
    // The existing one picked: syncing into it keeps its files; a new link no longer moves it.
    let form = sync_add_set_location(form, existing.clone());
    assert!(form.location_chosen);
    assert!(core.sync_add_window(form.clone()).location_note.unwrap().contains("The files in it stay"));
    let form = core.sync_add_set_link(form, link.clone());
    assert_eq!(form.location, existing);
    // The default picked again: it follows the link again.
    let default = form.default_location.clone().unwrap();
    assert!(!sync_add_set_location(form.clone(), default).location_chosen);
    // Choose…: a parent, the folder named after the link.
    let chosen = sync_add_choose_parent(form, "/tmp/elsewhere".into());
    let team = Path::new("/tmp/elsewhere").join("Team").display().to_string();
    assert_eq!(chosen.location, team);
    assert!(core.sync_add_window(chosen.clone()).locations.iter().any(|l| l.path == team));
    // Not a link: why, and no Start.
    let bad = core.sync_add_set_link(chosen, "hello".into());
    let w = core.sync_add_window(bad.clone());
    assert_eq!((w.link_line.as_str(), w.link_ok, w.can_start), ("Not a Seedwell folder link", false, false));
    assert_eq!(crate::sync_text::core_error_message(core.sync_add_start(bad).unwrap_err()), "Not a Seedwell folder link");
    // Joined.
    let j = core.sync_add_start(core.sync_add_form(Some(link))).unwrap();
    assert_eq!(j.state, SyncState::WaitingForAdmin);
}

#[test]
fn the_edit_dialog_saves_what_changed() {
    let (core, _d) = mock();
    let f = core.sync_add_folder("/tmp/seedwell-mock/Notes".into()).unwrap();
    let form = core.sync_edit_form(f.id.clone());
    let w = sync_edit_window(form.clone(), true);
    let computer = computer!();
    assert_eq!((w.title.as_str(), w.subtitle.clone()), ("Notes", Some(format!("Owner · {}", subtitle(&f)))));
    assert_eq!((w.location.as_str(), w.location_moved, w.move_enabled), ("/tmp/seedwell-mock", false, true));
    assert_eq!(w.location_help, Path::new("/tmp/seedwell-mock").join("Notes").display().to_string());
    assert_eq!(w.move_picker.message, "Choose where “Notes” goes. It keeps its name.");
    assert_eq!(w.share_title.as_deref(), Some("Share…"));
    assert_eq!(w.share_note, sync_edit_share_note(SyncRole::Owner));
    assert_eq!(w.lan_help, format!("Sync this folder only with devices on the same network as this {computer}, never over the Internet"));
    assert!(!w.changed && w.can_save && w.gone.is_none());
    // Without the Share dialog: no Share….
    assert_eq!(sync_edit_window(form.clone(), false).share_title, None);
    // Renamed, paused, local network only: saved in one go.
    let edited = SyncEditForm { name: " Journal ".into(), paused: true, lan_only: true, ..form.clone() };
    assert!(sync_edit_window(edited.clone(), true).changed);
    let saved = core.sync_edit_save(edited).unwrap();
    assert_eq!((saved.name.as_str(), saved.state, saved.lan_only), ("Journal", SyncState::Paused, true));
    // A name that cannot be used.
    let form = core.sync_edit_form(f.id.clone());
    let e = core.sync_edit_save(SyncEditForm { name: ".hidden".into(), ..form.clone() }).unwrap_err();
    assert_eq!(crate::sync_text::core_error_message(e), "A folder name cannot start with a dot.");
    // Move…: below the new parent, with its name.
    let moved = sync_edit_choose_parent(form, "/tmp/seedwell-mock/Other".into());
    let w = sync_edit_window(moved, true);
    assert!(w.location_moved && w.changed);
    assert_eq!(w.location, "/tmp/seedwell-mock/Other");
    // Gone: the dialog says so, and Save cannot be pressed.
    core.sync_remove(f.id.clone(), false).unwrap();
    let w = sync_edit_window(core.sync_edit_form(f.id), true);
    assert_eq!(w.gone, Some(format!("This folder is no longer synced on this {computer}.")));
    assert!(!w.can_save && w.title == "Folder" && w.subtitle.is_none());
}

#[test]
fn the_on_demand_note_says_what_the_switch_does() {
    let (core, _d) = mock();
    let f = core.sync_add_folder("/tmp/seedwell-mock/Docs".into()).unwrap();
    let form = core.sync_edit_form(f.id);
    assert_eq!(sync_edit_window(form.clone(), true).on_demand_note, None, "full stays full");
    let on = SyncEditForm { on_demand: true, ..form.clone() };
    assert_eq!(sync_edit_window(on, true).on_demand_note, Some(sync_on_demand_note(SyncOnDemandChange::TurnOn, String::new())));
    let mut od = form.clone();
    od.folder.as_mut().unwrap().on_demand = true;
    assert!(!sync_edit_window(od.clone(), true).move_enabled, "an on-demand folder stays where the file manager shows it");
    assert_eq!(sync_edit_window(od.clone(), true).on_demand_note, Some(sync_on_demand_note(SyncOnDemandChange::TurnOff, "/tmp/seedwell-mock/Docs".into())));
    let stays = SyncEditForm { on_demand: true, ..od };
    assert_eq!(sync_edit_window(stays, true).on_demand_note, Some(sync_on_demand_note(SyncOnDemandChange::Stays, "/tmp/seedwell-mock/Docs".into())));
}
