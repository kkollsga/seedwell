//! The file lists (SPEC 8.2, 8.3): open and closed lists, reading the files and when, the selection and its keys,
//! edits on their way to the engine, and what the window and the menu draw.

use super::*;
use crate::host::host_windows_all;
use crate::row_text::tests::row as plain_row;
use crate::types::TransferState;
use FilePriority::*;

fn file(i: u32, path: &str, len: u64, done: u64, p: FilePriority) -> TorrentFile {
    let name = path.rsplit('/').next().unwrap_or(path).to_string();
    TorrentFile {
        index: i,
        path: path.into(),
        disk_path: format!("/nowhere/{path}"),
        is_video: name.ends_with(".mkv"),
        name,
        length: len,
        done,
        uploaded: 0,
        wanted: p != Skip,
        priority: p,
        is_mp4_family: false,
    }
}

/// One folder: a sample (skipped) in Extras, two episodes, an .nfo.
fn season() -> Vec<TorrentFile> {
    vec![
        file(0, "Show S01/Show.S01E10.mkv", 300, 300, Normal),
        file(1, "Show S01/Show.S01E02.mkv", 200, 100, Normal),
        file(2, "Show S01/Extras/sample.mkv", 50, 0, Skip),
        file(3, "Show S01/info.nfo", 1, 1, Skip),
    ]
}

fn window() -> Arc<FileLists> {
    let l = FileLists::new(FileListPlace::Window);
    assert!(l.expand("t".into()), "a new list is read");
    l.loaded("t".into(), Some(season()), 0);
    l
}

fn names(l: &FileLists) -> Vec<String> {
    l.view("t".into(), false).unwrap().rows.iter().map(|r| r.file.name.clone()).collect()
}

fn node(l: &FileLists, name: &str) -> u32 {
    l.find("t".into(), name.into()).unwrap_or_else(|| panic!("no row {name}"))
}

#[test]
fn a_list_loads_then_shows_its_header_and_rows_with_one_top_folder_open() {
    let l = FileLists::new(FileListPlace::Window);
    assert!(l.view("t".into(), false).is_none(), "not open");
    l.expand("t".into());
    let v = l.view("t".into(), false).unwrap();
    assert_eq!((v.status.as_deref(), v.rows.len(), v.header.is_none()), (Some("Loading files…"), 0, true));
    assert_eq!((v.refresh_ms, v.flush_ms), (1_000, 250));
    l.loaded("t".into(), None, 0);
    assert_eq!(l.view("t".into(), false).unwrap().status.as_deref(), Some("Couldn't read the files."));
    l.loaded("t".into(), Some(season()), 0);
    let v = l.view("t".into(), false).unwrap();
    assert_eq!(v.status, None);
    assert_eq!(names(&l), ["Show S01", "Extras", "info.nfo", "Show.S01E02.mkv", "Show.S01E10.mkv"]);
    let h = v.header.unwrap();
    assert_eq!(h.summary, "2 of 4 files · 500 bytes of 551 bytes selected");
    assert_eq!(h.summary_help, "Skipped files stay on disk");
    assert!(h.filter.is_none() && h.show_all.is_none(), "a short list has neither");
    assert_eq!((h.select_all.title.as_str(), h.select_all.help.as_str(), h.select_all.enabled), ("Select All", "Download every file", true));
    assert_eq!((h.select_none.title.as_str(), h.select_none.help.as_str(), h.select_none.enabled), ("Select None", "Skip every file; skipped files stay on disk", true));
    assert_eq!((v.priority_help.as_str(), v.complete_label.as_str(), v.max_rows), ("Priority", "Complete", Some(13)));
}

#[test]
fn every_row_says_what_its_controls_do() {
    let l = window();
    let v = l.view("t".into(), false).unwrap();
    let r = |name: &str| v.rows.iter().find(|r| r.file.name == name).unwrap();
    let top = r("Show S01");
    assert_eq!((top.fold_help.as_str(), top.fold_label.as_str()), ("Collapse this folder", "Collapse"));
    assert_eq!((top.check_help.as_str(), top.check_value.as_str()), ("Download this", "Some files"));
    let extras = r("Extras");
    assert_eq!((extras.fold_help.as_str(), extras.fold_label.as_str()), ("Expand this folder", "Expand"));
    assert_eq!(extras.check_value, "Skipped");
    let e10 = r("Show.S01E10.mkv");
    assert_eq!((e10.check_help.as_str(), e10.check_value.as_str()), ("Skip this; the file stays on disk if already downloaded", "Downloading"));
    assert!(e10.fold_help.is_empty());
    assert_eq!(e10.size_label, "300 bytes, downloaded");
    assert_eq!(r("Show.S01E02.mkv").size_label, "200 bytes, 50 percent downloaded");
    assert_eq!(extras.size_label, "50 bytes");
    assert_eq!(e10.play.as_ref().map(|p| p.help.as_str()), Some("Play Show.S01E10.mkv"));
    assert!(r("info.nfo").play.is_none() && extras.play.is_none());
    assert_eq!(e10.priorities.iter().map(|p| (p.title.as_str(), p.checked)).collect::<Vec<_>>(), [("High", false), ("Normal", true), ("Low", false)]);
}

#[test]
fn a_host_that_cannot_play_gets_no_play_button() {
    let l = window();
    l.set_host_windows(host_windows_all().into_iter().filter(|w| *w != HostWindow::Player).collect());
    let v = l.view("t".into(), false).unwrap();
    assert!(v.rows.iter().all(|r| r.play.is_none()), "no Play on any row without a player window");
    l.set_host_windows(host_windows_all());
    let v = l.view("t".into(), false).unwrap();
    assert!(v.rows.iter().any(|r| r.play.is_some()), "with one, videos have it");
}

#[test]
fn a_filter_shows_the_matching_files_and_says_when_none_match() {
    let many: Vec<TorrentFile> = (0..25).map(|i| file(i, &format!("Album/{i:02} Track.flac"), 10, 0, Normal)).collect();
    let l = FileLists::new(FileListPlace::Window);
    l.expand("t".into());
    l.loaded("t".into(), Some(many), 0);
    let v = l.view("t".into(), false).unwrap();
    let h = v.header.unwrap();
    let f = h.filter.unwrap();
    assert_eq!((f.placeholder.as_str(), f.clear_help.as_str(), f.clear_label.as_str()), ("Filter files", "Clear the file filter", "Clear filter"));
    assert_eq!(h.show_all.map(|b| b.title), Some("Show All".into()), "26 rows overflow the 13 shown");
    assert!(!h.select_all.enabled, "everything is wanted already");
    l.set_filter("t".into(), "07".into());
    let v = l.view("t".into(), false).unwrap();
    assert_eq!(v.rows.iter().map(|r| r.file.name.as_str()).collect::<Vec<_>>(), ["Album", "07 Track.flac"]);
    assert!(v.header.unwrap().select_all.enabled, "with a filter it acts on what it shows");
    l.set_filter("t".into(), "zzz".into());
    assert_eq!(l.view("t".into(), false).unwrap().empty.as_deref(), Some("No files match."));
    let v = l.view("t".into(), true).unwrap();
    assert_eq!(v.header.unwrap().show_all.map(|b| (b.title, b.help)), Some(("Show Less".into(), "Scroll long file lists inside their row again".into())));
    assert_eq!(v.max_rows, Some(13), "no rows: nothing to grow");
    l.set_filter("t".into(), String::new());
    assert_eq!(l.view("t".into(), true).unwrap().max_rows, None, "Show All: as tall as its rows");
    let empty = FileLists::new(FileListPlace::Window);
    empty.expand("e".into());
    empty.loaded("e".into(), Some(vec![]), 0);
    assert_eq!(empty.view("e".into(), false).unwrap().empty.as_deref(), Some("No files."));
}

#[test]
fn select_all_and_none_cover_what_the_filter_shows() {
    let l = window();
    assert!(l.select_all("t".into(), false));
    assert_eq!(l.view("t".into(), false).unwrap().header.unwrap().wanted_count, 0);
    l.set_filter("t".into(), "E02".into());
    assert!(l.select_all("t".into(), true));
    let h = l.view("t".into(), false).unwrap().header.unwrap();
    assert_eq!((h.wanted_count, h.file_count), (1, 4));
    assert!(!l.select_all("t".into(), true), "nothing left to change");
}

#[test]
fn clicks_select_one_row_command_adds_or_removes_and_shift_takes_the_range() {
    let l = window();
    let (e02, e10, nfo) = (node(&l, "S01E02"), node(&l, "S01E10"), node(&l, "info.nfo"));
    l.click("t".into(), e02, false, false);
    assert_eq!(l.selection(), Some(FileSelection { torrent_id: "t".into(), nodes: vec![e02] }));
    for _ in 0..5 {
        l.click("t".into(), nfo, true, false);
    }
    assert_eq!(l.selection().unwrap().nodes, [e02, nfo].into_iter().collect::<BTreeSet<_>>().into_iter().collect::<Vec<_>>());
    l.click("t".into(), nfo, true, false);
    assert_eq!(l.selection().unwrap().nodes, [e02]);
    l.click("t".into(), e02, false, false);
    l.click("t".into(), nfo, false, true);
    let rows: Vec<u32> = l.view("t".into(), false).unwrap().rows.iter().filter(|r| r.selected).map(|r| r.file.id).collect();
    assert_eq!(rows.len(), 2, "info.nfo and E02 are next to each other: {rows:?}");
    l.click("t".into(), e10, false, true);
    assert_eq!(l.selection().unwrap().nodes.len(), 2, "the range from the last plain click (E02), to E10");
    l.clear_selection();
    assert_eq!(l.selection(), None);
    l.click("t".into(), e10, false, true);
    assert_eq!(l.selection().unwrap().nodes, [e10], "no anchor: a plain select");
}

#[test]
fn another_list_starts_fresh_and_closing_a_list_drops_only_its_selection() {
    let l = window();
    l.expand("u".into());
    l.loaded("u".into(), Some(season()), 0);
    let e02 = node(&l, "S01E02");
    l.click("t".into(), e02, false, false);
    l.click("u".into(), e02, true, false);
    assert_eq!(l.selection(), Some(FileSelection { torrent_id: "u".into(), nodes: vec![e02] }));
    l.collapse("t".into());
    assert!(l.selection().is_some());
    l.collapse("u".into());
    assert_eq!(l.selection(), None);
    for _ in 0..50 {
        l.toggle("t".into());
    }
    assert!(!l.is_expanded("t".into()) && l.view("t".into(), false).is_none());
    assert!(l.toggle("t".into()), "opened again: read again");
    assert_eq!(l.expanded(), ["t"]);
}

#[test]
fn an_action_on_a_selected_row_covers_the_selection() {
    let l = window();
    let (e02, e10) = (node(&l, "S01E02"), node(&l, "S01E10"));
    l.click("t".into(), e02, false, false);
    l.click("t".into(), e10, true, false);
    assert!(l.toggle_check("t".into(), e10));
    let h = l.view("t".into(), false).unwrap().header.unwrap();
    assert_eq!(h.wanted_count, 0, "both skipped");
    // A row outside the selection: only that row.
    let nfo = node(&l, "info.nfo");
    l.toggle_check("t".into(), nfo);
    assert_eq!(l.view("t".into(), false).unwrap().header.unwrap().wanted_count, 1);
    assert!(l.set_priority("t".into(), e02, High));
    let v = l.view("t".into(), false).unwrap();
    assert_eq!(v.rows.iter().filter(|r| r.file.priority_label == "High").count(), 2, "none of them was wanted: all of them change (the folder is mixed)");
}

#[test]
fn space_and_the_arrows_act_on_the_selected_rows() {
    let l = window();
    assert_eq!(l.space(), None, "nothing selected");
    let (top, extras, sample) = (node(&l, "Show S01"), node(&l, "Extras"), node(&l, "sample"));
    l.click("t".into(), extras, false, false);
    assert!(l.right(), "→ opens the folder");
    assert!(names(&l).contains(&"sample.mkv".to_string()));
    assert_eq!(l.space().as_deref(), Some("t"));
    assert_eq!(l.view("t".into(), false).unwrap().header.unwrap().wanted_count, 3);
    l.click("t".into(), sample, false, false);
    assert_eq!(l.left(), FileLeft { handled: true, select_torrent: None }, "a file: its folder is selected");
    assert_eq!(l.selection().unwrap().nodes, [extras]);
    assert_eq!(l.left(), FileLeft { handled: true, select_torrent: None }, "an open folder closes");
    assert!(!names(&l).contains(&"sample.mkv".to_string()));
    assert_eq!(l.left(), FileLeft { handled: true, select_torrent: None }, "a closed folder: the folder it is in");
    assert_eq!(l.selection().unwrap().nodes, [top]);
    assert_eq!(l.left(), FileLeft { handled: true, select_torrent: None }, "the top folder closes");
    assert_eq!(l.left(), FileLeft { handled: true, select_torrent: Some("t".into()) }, "the top: the torrent's own row");
    assert_eq!(l.selection(), None);
    assert_eq!(l.left(), FileLeft { handled: false, select_torrent: None });
}

#[test]
fn edits_show_at_once_and_go_out_together_and_the_list_is_not_read_meanwhile() {
    let l = window();
    let (e02, e10) = (node(&l, "S01E02"), node(&l, "S01E10"));
    assert!(l.due(5_000).contains(&"t".to_string()), "read once a second");
    assert!(!l.read("t".into(), Some(season()), 5_000), "nothing moved");
    l.toggle_check("t".into(), e02);
    l.toggle_check("t".into(), e10);
    assert!(l.has_pending("t".into()));
    assert!(l.due(10_000).is_empty(), "not while edits are on their way");
    let edits = l.take_edits("t".into()).unwrap();
    assert_eq!(edits.changes.len(), 2, "a run of clicks is one change");
    assert!(!edits.ticked);
    assert_eq!(l.take_edits("t".into()), None);
    // The engine's old answer does not undo them on screen.
    assert!(!l.read("t".into(), Some(season()), 10_500));
    assert_eq!(l.view("t".into(), false).unwrap().header.unwrap().wanted_count, 0);
    assert_eq!(l.sent("t".into(), true), FileListRead::Read);
    assert!(!l.has_pending("t".into()));
    // Ticking: a finished torrent may become a download again.
    l.toggle_check("t".into(), e02);
    assert!(l.take_edits("t".into()).unwrap().ticked);
    assert_eq!(l.sent("t".into(), false), FileListRead::Reload, "refused: everything again");
}

#[test]
fn a_list_closed_with_edits_on_their_way_keeps_them_until_they_are_sent() {
    let l = window();
    l.toggle_check("t".into(), node(&l, "S01E02"));
    l.collapse("t".into());
    assert!(l.has_pending("t".into()));
    let edits = l.take_edits("t".into()).unwrap();
    assert_eq!(edits.changes, [FilePriorityChange { index: 1, priority: Skip }]);
    assert_eq!(l.sent("t".into(), true), FileListRead::Nothing);
    assert!(!l.has_pending("t".into()));
    assert!(l.expand("t".into()), "gone: read again when opened");
}

#[test]
fn the_window_closing_forgets_the_lists_and_opening_reads_them_again() {
    let l = window();
    l.click("t".into(), node(&l, "S01E02"), false, false);
    l.suspend();
    assert_eq!(l.selection(), None);
    assert!(l.is_expanded("t".into()));
    assert!(l.view("t".into(), false).is_none(), "nothing is kept");
    assert_eq!(l.resume(), ["t"]);
    assert_eq!(l.view("t".into(), false).unwrap().status.as_deref(), Some("Loading files…"));
    l.collapse_all();
    assert!(l.expanded().is_empty());
}

#[test]
fn a_read_finds_new_numbers_and_rebuilds_a_list_that_changed_shape() {
    let l = window();
    let mut more = season();
    more[1].done = 200;
    l.due(1_000);
    assert!(l.read("t".into(), Some(more.clone()), 1_000));
    assert!(l.due(1_500).is_empty(), "not again within the second");
    l.due(2_000);
    more.push(file(4, "Show S01/new.mkv", 5, 0, Normal));
    assert!(l.read("t".into(), Some(more), 2_000));
    assert!(names(&l).contains(&"new.mkv".to_string()));
    assert!(!l.read("t".into(), None, 3_000), "a failed read changes nothing");
}

#[test]
fn a_file_rows_menu_reveals_plays_and_sets_the_priority() {
    let l = window();
    let e10 = node(&l, "S01E10");
    let r = plain_row("t", TransferState::Downloading);
    let players = vec![PlayerOption { id: "iina".into(), name: "IINA".into(), runs: true, mp4_only: false }];
    let m = l.menu("t".into(), e10, r.clone(), players.clone(), host_windows_all());
    assert_eq!(m[0], FileMenuEntry::Item { title: concat!("Show in ", file_manager!()).into(), command: FileCommand::Reveal { path: None } });
    assert_eq!(m[1], FileMenuEntry::Item { title: "Play".into(), command: FileCommand::PlayFile { node: e10, player_id: None } });
    assert_eq!(m[2], FileMenuEntry::Submenu {
        title: "Play With".into(),
        entries: vec![FileMenuEntry::Item { title: "IINA".into(), command: FileCommand::PlayFile { node: e10, player_id: Some("iina".into()) } }]
    });
    assert_eq!(m[3], FileMenuEntry::Separator);
    let FileMenuEntry::Submenu { title, entries } = &m[4] else { panic!("{m:?}") };
    assert_eq!(title, "Priority");
    let titles: Vec<String> = entries
        .iter()
        .map(|e| match e {
            FileMenuEntry::Item { title, .. } => title.clone(),
            FileMenuEntry::Separator => "-".into(),
            _ => "?".into(),
        })
        .collect();
    assert_eq!(titles, ["High", "Normal", "Low", "-", "Skip"]);
    // Without a player window, no Play; a file that is on disk is shown itself.
    let dir = tempfile::tempdir().unwrap();
    let mut files = season();
    files[0].disk_path = dir.path().join("e10.mkv").display().to_string();
    std::fs::write(&files[0].disk_path, b"x").unwrap();
    l.loaded("t".into(), Some(files.clone()), 0);
    let m = l.menu("t".into(), e10, r, players, vec![HostWindow::FileList]);
    assert_eq!(m.len(), 3);
    assert_eq!(m[0], FileMenuEntry::Item { title: concat!("Show in ", file_manager!()).into(), command: FileCommand::Reveal { path: Some(files[0].disk_path.clone()) } });
    assert_eq!(l.file("t".into(), e10).map(|f| f.index), Some(0));
    assert_eq!(l.file("t".into(), node(&l, "Extras")), None, "a folder is no file");
}

#[test]
fn the_menu_keeps_one_list_open_without_a_selection() {
    let l = FileLists::new(FileListPlace::Menu);
    l.expand("a".into());
    l.loaded("a".into(), Some(season()), 0);
    let v = l.menu_view("a".into()).unwrap();
    // Small: every folder open, the lone wrapper folder stepped into.
    assert_eq!(v.rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["Extras", "sample.mkv", "info.nfo", "Show.S01E02.mkv", "Show.S01E10.mkv"]);
    assert_eq!((v.footer.as_str(), v.more.is_none(), v.reserves_play, v.height_rows), ("2 of 4 files · 500 bytes of 551 bytes", true, true, 6));
    let e02 = l.find("a".into(), "E02".into()).unwrap();
    assert!(l.toggle_check("a".into(), e02));
    assert_eq!(l.menu_view("a".into()).unwrap().footer, "1 of 4 files · 300 bytes of 551 bytes");
    l.toggle_folder("a".into(), l.find("a".into(), "Extras".into()).unwrap());
    assert_eq!(l.menu_view("a".into()).unwrap().rows.len(), 4);
    // Another list: this one closes, its edits still go out.
    l.expand("b".into());
    assert_eq!(l.expanded(), ["b"]);
    assert!(l.take_edits("a".into()).is_some());
    let many: Vec<TorrentFile> = (0..600).map(|i| file(i, &format!("x/{i}.bin"), 1, 0, Normal)).collect();
    l.loaded("b".into(), Some(many), 0);
    let v = l.menu_view("b".into()).unwrap();
    assert_eq!((v.rows.len(), v.more.as_deref(), v.height_rows), (500, Some("Show All in Seedwell… (100 more)"), 501));
}

#[test]
fn the_rows_triangle_and_show_all_say_what_they_do() {
    assert_eq!(file_list_toggle(false), FileListToggle { help: "Show the files".into(), label: "Show files".into() });
    assert_eq!(file_list_toggle(true), FileListToggle { help: "Hide the files".into(), label: "Hide files".into() });
    assert_eq!(file_list_show_all(false).title, "Show All");
    assert_eq!(file_list_show_all(false).help, "Grow long file lists to their full height instead of scrolling inside");
}
