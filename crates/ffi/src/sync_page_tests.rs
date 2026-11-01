use super::*;
use crate::chat_text::tests::channel;
use crate::host::host_windows_all;
use crate::library::tests::Clock;
use crate::sync::{SyncGoneDevice, SyncIssue};
use crate::sync_text::tests::folder;

const NOW_S: u64 = 1_800_000_000;
/// What Windows passed in build 37.
const WINDOWS: [HostWindow; 4] = [HostWindow::GetInfo, HostWindow::MoveTo, HostWindow::NewChannel, HostWindow::ChatDialogs];

fn snap(folders: Vec<SyncFolder>) -> SyncSnapshot {
    SyncSnapshot { running: true, problem: None, device_id: "D".into(), device_name: "Mac".into(), port: 22100, folders, devices: Vec::new() }
}

fn input(folders: Vec<SyncFolder>) -> SyncPageInput {
    SyncPageInput {
        snapshot: snap(folders),
        selected: None,
        members: Vec::new(),
        conflicts: Vec::new(),
        channels: Vec::new(),
        on_demand_off: false,
        now_unix_s: NOW_S,
    }
}

fn model(i: &SyncPageInput, host: &[HostWindow]) -> SyncPageModel {
    sync_page_model(i.clone(), SyncRemoval::new(), Arc::new(Clock), host.to_vec())
}

fn titles(entries: &[SyncPageMenuEntry]) -> Vec<String> {
    entries
        .iter()
        .map(|e| match e {
            SyncPageMenuEntry::Item { title, .. } => title.clone(),
            SyncPageMenuEntry::Separator => "-".into(),
        })
        .collect()
}

fn member(id: &str, name: &str, role: SyncRole, is_me: bool) -> SyncMember {
    SyncMember { device_id: id.into(), name: name.into(), user: String::new(), role, online: false, is_me, expires_s: None }
}

#[test]
fn no_folders_is_one_line_and_add_folder() {
    let m = model(&input(Vec::new()), &host_windows_all());
    let e = m.empty.unwrap();
    assert_eq!(e.text, "Keep folders the same on your devices");
    assert_eq!(e.add.unwrap().title, "Add Folder…");
    assert!(m.folders.is_empty() && m.page.is_none() && m.selected.is_none());
    // Sync could not start: why, instead.
    let mut i = input(Vec::new());
    i.snapshot.running = false;
    i.snapshot.problem = Some("The port is taken.".into());
    assert_eq!(model(&i, &host_windows_all()).empty.unwrap().text, "The port is taken.");
    // Without the Add Folder dialog: no button.
    assert_eq!(model(&input(Vec::new()), &WINDOWS).empty.unwrap().add, None);
}

#[test]
fn the_selection_falls_back_to_the_first_folder() {
    let mut i = input(vec![folder("A", SyncState::UpToDate), folder("B", SyncState::UpToDate)]);
    let m = model(&i, &host_windows_all());
    assert_eq!(m.selected.as_deref(), Some("ID-A"));
    assert_eq!(m.folders.iter().map(|f| f.selected).collect::<Vec<_>>(), [true, false]);
    i.selected = Some("ID-B".into());
    assert_eq!(model(&i, &host_windows_all()).page.unwrap().title, "B");
    i.selected = Some("gone".into());
    assert_eq!(model(&i, &host_windows_all()).selected.as_deref(), Some("ID-A"));
}

#[test]
fn the_list_rows_and_their_menus() {
    let mut a = folder("Photos", SyncState::UpToDate);
    a.channel_id = Some("Family".into());
    let mut b = folder("Shared", SyncState::UpToDate);
    b.role = SyncRole::ReadWrite;
    let mut i = input(vec![a, b]);
    i.channels = vec![channel("Family", 2, 1, 0, false)];
    let m = model(&i, &host_windows_all());
    let r = &m.folders[0];
    assert_eq!((r.title.as_str(), r.subtitle.as_str(), r.accessibility_label.as_str()), ("Photos", "Up to date", "Photos, Up to date"));
    let reveal = concat!("Show in ", trs_platform::file_manager!());
    assert_eq!(titles(&r.menu), [reveal, "Share…", "Open Chat", "Edit…", "-", "Stop Syncing…"]);
    assert_eq!(titles(&m.folders[1].menu), [reveal, "Edit…", "-", "Stop Syncing…"], "Read & write may not share; no channel here");
    assert_eq!(m.add.unwrap().title, "Add Folder…");
    // Windows' set: no Share…, no Edit…, no Add Folder….
    let w = model(&i, &WINDOWS);
    assert_eq!(titles(&w.folders[0].menu), [reveal, "Open Chat", "-", "Stop Syncing…"]);
    assert!(w.add.is_none());
}

#[test]
fn the_header_has_its_buttons_place_and_status() {
    let mut f = folder("Photos", SyncState::UpToDate);
    f.channel_id = Some("Family".into());
    f.share_until_s = Some(NOW_S + 86_400);
    f.last_synced_s = Some(NOW_S - 180);
    f.connected = 2;
    let mut i = input(vec![f]);
    i.channels = vec![channel("Family", 2, 1, 0, false)];
    let p = model(&i, &host_windows_all()).page.unwrap();
    let b: Vec<(&str, &str)> = p.buttons.iter().map(|b| (b.title.as_str(), b.help.as_str())).collect();
    let computer = trs_platform::computer!();
    assert_eq!(
        b,
        [
            ("Chat", "Open “Family”, this folder’s channel"),
            ("Share…", format!("Make a link that lets another {computer} join this folder").as_str()),
            ("Edit…", "Rename or move the folder, options"),
            ("Stop Syncing…", format!("Stop syncing this folder on this {computer}; other devices keep their copy").as_str()),
        ]
    );
    assert_eq!(p.buttons[0].command, SyncPageCommand::OpenChat { channel_id: "Family".into() });
    assert_eq!(p.place, "/Users/me/Seedwell/Photos");
    assert_eq!(p.reveal.accessibility_label.as_deref(), Some(format!("Show Photos in {}", trs_platform::file_manager!()).as_str()));
    assert_eq!(p.status, format!("Up to date · 2 devices · Owner · shared until D{} · 10 items · 1.0 GB · synced 3 minutes ago", NOW_S + 86_400));
    assert_eq!((p.error, p.note, p.on_demand_off), (None, None, None));
    let w = model(&i, &WINDOWS).page.unwrap();
    assert_eq!(w.buttons.iter().map(|b| b.title.as_str()).collect::<Vec<_>>(), ["Chat", "Stop Syncing…"]);
}

#[test]
fn a_missing_a_failing_and_a_hidden_on_demand_folder_say_so() {
    let p = model(&input(vec![folder("Gone", SyncState::Missing)]), &host_windows_all()).page.unwrap();
    assert!(p.note.unwrap().starts_with("The folder or its disk is not there."));
    let mut bad = folder("Bad", SyncState::Error);
    bad.error = Some("Disk full".into());
    assert_eq!(model(&input(vec![bad]), &host_windows_all()).page.unwrap().error.as_deref(), Some("Disk full"));
    let mut od = folder("Docs", SyncState::UpToDate);
    od.on_demand = true;
    let mut i = input(vec![od]);
    assert_eq!(model(&i, &host_windows_all()).page.unwrap().on_demand_off, None);
    i.on_demand_off = true;
    let n = model(&i, &host_windows_all()).page.unwrap().on_demand_off.unwrap();
    assert_eq!(n.text, sync_on_demand_off_line());
    assert_eq!(n.button.unwrap().command, SyncPageCommand::AllowOnDemand);
}

#[test]
fn members_are_ordered_named_and_an_owner_gets_their_menu() {
    let mut i = input(vec![folder("Photos", SyncState::UpToDate)]);
    i.members = vec![member("B", "Studio", SyncRole::ReadOnly, false), member("M", "", SyncRole::Owner, true), member("A", "Anna", SyncRole::Admin, false)];
    i.members[2].user = "Anna K".into();
    i.members[2].online = true;
    i.snapshot.devices = vec![SyncDevice {
        id: "A".into(),
        name: "Anna".into(),
        client: "Seedwell 0.1.14".into(),
        address: String::new(),
        via: "LAN".into(),
        folders: vec!["ID-Photos".into()],
        rx_bytes: 12_000_000,
        tx_bytes: 3_000_000,
    }];
    let p = model(&i, &host_windows_all()).page.unwrap();
    assert_eq!(p.members.title, "Members");
    let computer = trs_platform::computer!();
    let rows = &p.members.rows;
    assert_eq!(rows.iter().map(|r| r.name.clone()).collect::<Vec<_>>(), [format!("This {computer} (this {computer})"), "Anna K (Anna)".into(), "Studio".into()]);
    assert_eq!(rows.iter().map(|r| r.reach).collect::<Vec<_>>(), [SyncMemberReach::ThisDevice, SyncMemberReach::LocalNetwork, SyncMemberReach::Away]);
    assert_eq!(rows[1].line, "Local network · Seedwell 0.1.14");
    assert_eq!(rows[1].transfer.as_deref(), Some("↓ 12 MB   ↑ 3.0 MB"));
    assert!(rows[0].role_menu.is_none(), "not for this computer");
    let menu = rows[2].role_menu.clone().unwrap();
    assert_eq!((menu.title.as_str(), menu.help.as_str()), ("Read only", "Change what Studio may do in this folder, or remove it"));
    assert_eq!(titles(&menu.entries), ["Owner", "Admin", "Read & write", "Read only", "-", "Remove from Folder…"]);
    let SyncPageMenuEntry::Item { checked, enabled, .. } = &menu.entries[3] else { panic!() };
    assert!(*checked && !*enabled, "the role in force is checked and cannot be picked");
    let SyncPageMenuEntry::Item { command, question, .. } = &menu.entries[5] else { panic!() };
    assert_eq!(*command, SyncPageCommand::RemoveDevice { device_id: "B".into() });
    let q = question.clone().unwrap();
    assert_eq!((q.title.as_str(), q.confirm_title.as_str(), q.destructive), ("Remove Studio from “Photos”?", "Remove", true));
    assert_eq!(q.text, "It stops syncing at once, on every device. It keeps the files it already has.");
    // Not an owner: the role is plain text.
    let mut admin = i.clone();
    admin.snapshot.folders[0].role = SyncRole::Admin;
    assert!(model(&admin, &host_windows_all()).page.unwrap().members.rows.iter().all(|r| r.role_menu.is_none()));
}

#[test]
fn no_members_yet_says_what_the_join_does() {
    let p = model(&input(vec![folder("A", SyncState::UpToDate)]), &host_windows_all()).page.unwrap();
    assert_eq!(p.members.empty.as_deref(), Some("No members known yet"));
    let mut joining = folder("J", SyncState::WaitingForAdmin);
    joining.progress = Some("Looking for Anna's Mac mini on the local network…".into());
    let p = model(&input(vec![joining.clone()]), &host_windows_all()).page.unwrap();
    assert_eq!(p.members.empty, Some(join_note(&joining)));
}

#[test]
fn conflicts_and_issues_have_their_sections_and_actions() {
    let mut f = folder("Docs", SyncState::UpToDate);
    f.conflicts = 2;
    f.issues = vec![SyncIssue { path: "a\rb".into(), reason: "Not a valid name on Windows".into() }, SyncIssue { path: String::new(), reason: "Read only disk".into() }];
    let mut i = input(vec![f]);
    let p = model(&i, &host_windows_all()).page.unwrap();
    let c = p.conflicts.unwrap();
    assert_eq!((c.title.as_str(), c.empty.as_deref(), c.rows.len()), ("Conflicts", Some("2 conflict copies"), 0), "counted until they are read");
    let issues = p.issues.unwrap();
    assert_eq!(issues.title, "Issues");
    assert_eq!(issues.rows.iter().map(|r| r.path.clone()).collect::<Vec<_>>(), [Some("a␍b".to_string()), None]);
    i.conflicts = vec![
        SyncConflict { path: "/x/R (conflict).txt".into(), copy: "R (conflict).txt".into(), original: "R.txt".into(), device: "MacBook".into(), when: "2026-10-25 15.32".into() },
        SyncConflict { path: "/x/S (conflict).txt".into(), copy: "S (conflict).txt".into(), original: "S.txt".into(), device: String::new(), when: "2026-10-25 15.33".into() },
    ];
    let c = model(&i, &host_windows_all()).page.unwrap().conflicts.unwrap();
    assert_eq!(c.empty, None);
    let r = &c.rows[0];
    assert_eq!((r.title.as_str(), r.line.as_str()), ("R.txt", "MacBook’s version · 2026-10-25 15.32"));
    assert_eq!(r.reveal.command, SyncPageCommand::RevealConflict { path: "/x/R (conflict).txt".into() });
    assert_eq!(titles(&r.keep.entries), ["Current Version", "MacBook’s Version", "Both"]);
    assert_eq!(titles(&c.rows[1].keep.entries)[1], "The Copy");
    let SyncPageMenuEntry::Item { command, .. } = &r.keep.entries[1] else { panic!() };
    assert_eq!(*command, SyncPageCommand::Resolve { copy: "R (conflict).txt".into(), keep: SyncKeep::Copy });
    // None left: no section.
    let mut clean = i.clone();
    clean.snapshot.folders[0].conflicts = 0;
    clean.conflicts.clear();
    clean.snapshot.folders[0].issues.clear();
    let p = model(&clean, &host_windows_all()).page.unwrap();
    assert!(p.conflicts.is_none() && p.issues.is_none());
}

#[test]
fn gone_devices_and_share_again_ask_first() {
    let mut f = folder("Photos", SyncState::UpToDate);
    f.gone = vec![SyncGoneDevice { device_id: "G".into(), name: "Mac mini".into(), last_seen_s: Some(NOW_S - 3 * 86_400) }];
    f.can_reshare = true;
    let p = model(&input(vec![f.clone()]), &host_windows_all()).page.unwrap();
    let g = &p.gone[0];
    assert_eq!(g.line, "Mac mini was last seen 3 days ago");
    assert_eq!((g.button.title.as_str(), g.button.command.clone()), ("Remove from Folder…", SyncPageCommand::RemoveDevice { device_id: "G".into() }));
    let q = g.button.question.clone().unwrap();
    assert_eq!((q.title.as_str(), q.destructive), ("Remove Mac mini from “Photos”?", true));
    let r = p.reshare.unwrap();
    let prompt = sync_reshare_prompt(f);
    assert_eq!((r.note, r.button.title, r.button.command), (prompt.note, prompt.button, SyncPageCommand::Reshare));
    let q = r.button.question.unwrap();
    assert_eq!((q.title, q.confirm_title, q.destructive), (prompt.title, prompt.confirm, false));
}

#[test]
fn the_stop_syncing_question_opens_under_its_folder_and_goes_with_the_selection() {
    let mut i = input(vec![folder("A", SyncState::UpToDate), folder("B", SyncState::UpToDate)]);
    i.selected = Some("ID-B".into());
    let removal = SyncRemoval::new();
    let page = |i: &SyncPageInput| sync_page_model(i.clone(), removal.clone(), Arc::new(Clock), host_windows_all()).page.unwrap();
    assert!(page(&i).removal.is_none());
    removal.open("ID-B".into(), 3);
    removal.open("ID-B".into(), 0);
    removal.set_trash(true);
    let q = page(&i).removal.unwrap();
    assert_eq!(q.question, sync_remove_folder_question(folder("B", SyncState::UpToDate), 3), "opening it again keeps it as it is");
    assert_eq!((q.trash, q.remove_help), (true, sync_remove_help(true)));
    assert_eq!((q.cancel_title.as_str(), q.cancel_help.as_str(), q.remove_title.as_str()), ("Cancel", "Keep syncing this folder", "Remove"));
    // Another folder selected: it closes.
    i.selected = Some("ID-A".into());
    assert!(page(&i).removal.is_none());
    assert_eq!(removal.open_folder(), None);
}

#[test]
fn the_signature_follows_the_page() {
    let i = input(vec![folder("A", SyncState::UpToDate)]);
    let a = model(&i, &host_windows_all()).signature;
    assert_eq!(model(&i, &host_windows_all()).signature, a);
    let mut busy = i.clone();
    busy.snapshot.folders[0].state = SyncState::Paused;
    assert_ne!(model(&busy, &host_windows_all()).signature, a);
}
