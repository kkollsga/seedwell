use super::*;
use crate::library::tests::{inputs, library, mock, named, on, seeding, sharing, snap, state, t1, tracker, Clock, NOW_MS};
use crate::library::{build, Inputs};
use crate::library_rows::tests::{download, seed};
use crate::library_rows::SharingRows;
use crate::types::{SeedGroup, TransferState};

/// The window for `i` as `st` asks, settled as the object settles it after each model.
fn shown(i: &Inputs, st: &mut LibraryState) -> LibraryViewModel {
    let m = build(i, st, NOW_MS, &Clock);
    st.settle(&m);
    m
}

fn order(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs.iter().map(|(g, i)| (g.to_string(), i.to_string())).collect()
}

fn set(ids: &[&str]) -> BTreeSet<String> {
    ids.iter().map(|s| s.to_string()).collect()
}

/// Downloads with "a" and "b" downloading, "q" up next and "r" recently finished.
fn downloads() -> Inputs {
    let mut s = snap();
    s.downloading = vec![named("a", download(TransferState::Downloading)), named("b", download(TransferState::Downloading))];
    s.queued = vec![named("q", download(TransferState::Queued))];
    s.recent = vec![named("r", seed())];
    inputs(s)
}

fn ctx() -> WindowContext {
    WindowContext::default()
}

const NONE: KeyModifiers = KeyModifiers { shift: false, command: false, option: false, control: false };
const SHIFT: KeyModifiers = KeyModifiers { shift: true, ..NONE };
const COMMAND: KeyModifiers = KeyModifiers { command: true, ..NONE };

/// A key with the selected rows the fixture lists.
fn press(st: &mut LibraryState, i: &Inputs, key: LibraryKey, mods: KeyModifiers, scope: KeyScope, c: &WindowContext) -> (KeyOutcome, Option<RemovalQuestion>) {
    st.key(key, mods, scope, c, |st| {
        let mut seen = BTreeSet::new();
        st.listed.iter().filter(|(_, id)| st.selection.contains(id) && seen.insert(id.clone())).filter_map(|(_, id)| i.find(id).cloned()).collect()
    })
}

// MARK: Selection

#[test]
fn a_shift_click_range_follows_the_list_order_across_groups() {
    let o = order(&[("recent", "a"), ("recent", "b"), ("popular", "c"), ("popular", "d"), ("dormant", "e")]);
    assert_eq!(range(&o, Some("b"), "dormant", "e"), set(&["b", "c", "d", "e"]));
    assert_eq!(range(&o, Some("d"), "recent", "a"), set(&["a", "b", "c", "d"]));
    assert_eq!(range(&o, None, "popular", "c"), set(&["c"]));
}

#[test]
fn a_row_listed_twice_uses_the_occurrence_clicked() {
    let o = order(&[("recent", "a"), ("recent", "b"), ("popular", "c"), ("active", "a"), ("active", "d")]);
    assert_eq!(range(&o, Some("b"), "active", "a"), set(&["b", "c", "a"]));
    assert_eq!(range(&o, Some("b"), "recent", "a"), set(&["a", "b"]));
}

#[test]
fn the_anchor_follows_plain_and_command_clicks() {
    let mut st = state(LibraryPlace::Downloads);
    st.listed = order(&[("g", "a"), ("g", "b"), ("g", "c"), ("g", "d")]);
    st.click("g", "b", false, false);
    assert_eq!((st.selection.clone(), st.anchor.as_deref()), (set(&["b"]), Some("b")));
    st.click("g", "d", true, false);
    assert_eq!((st.selection.clone(), st.anchor.as_deref()), (set(&["b", "d"]), Some("d")));
    st.click("g", "a", false, true);
    assert_eq!((st.selection.clone(), st.anchor.as_deref()), (set(&["a", "b", "c", "d"]), Some("d")), "⇧ keeps the anchor");
    st.click("g", "d", true, false);
    assert!(!st.selection.contains("d") && st.anchor.as_deref() == Some("d"), "⌘ on the anchor deselects it; the anchor moves anyway");
    // An anchor no longer on screen: ⇧-click selects the row alone.
    st.anchor = Some("gone".into());
    st.click("g", "c", false, true);
    assert_eq!((st.selection.clone(), st.anchor.as_deref()), (set(&["c"]), Some("c")));
}

#[test]
fn the_model_marks_the_selected_rows_and_lists_them_in_order() {
    let i = downloads();
    let mut st = state(LibraryPlace::Downloads);
    let before = shown(&i, &mut st);
    st.click("active", "b", false, false);
    st.click("recent", "r", true, false);
    st.click("active", "a", true, false);
    st.info = Some("b".into());
    let m = shown(&i, &mut st);
    assert_eq!(m.selection, ["a", "b", "r"]);
    let row = |m: &LibraryViewModel, id: &str| m.list.as_ref().unwrap().groups.iter().flat_map(|g| g.rows.clone()).find(|r| r.id == id).unwrap();
    assert!(row(&m, "a").selected && !row(&m, "q").selected && !row(&m, "a").info_open && row(&m, "b").info_open);
    assert_ne!(row(&m, "a").revision, row(&before, "a").revision, "the revision follows the highlight");
    assert_eq!(row(&m, "q").revision, row(&before, "q").revision);
    assert_eq!(m.signature, before.signature, "the selection is not structure");
}

#[test]
fn rows_that_leave_the_screen_leave_the_selection() {
    let i = downloads();
    let mut st = state(LibraryPlace::Downloads);
    shown(&i, &mut st);
    st.click("active", "a", false, false);
    st.click("next", "q", true, false);
    st.info = Some("q".into());
    // Up Next folded: its row goes from the selection, the anchor and Get Info (Delete never acts on what is hidden).
    st.folded.insert("downloads.next".into());
    let m = shown(&i, &mut st);
    assert_eq!((m.selection.as_slice(), st.anchor.as_deref(), st.info.as_deref()), (&["a".to_string()][..], None, None));
    // A search that hides a row, and a removed torrent, likewise.
    st.folded.clear();
    st.selection = set(&["a", "b", "q"]);
    st.search = "b".into();
    assert_eq!(shown(&i, &mut st).selection, ["b"]);
    st.search.clear();
    let mut gone = downloads();
    gone.snapshot.downloading.remove(1);
    assert!(shown(&gone, &mut st).selection.is_empty());
}

#[test]
fn another_place_starts_with_nothing_selected() {
    let i = downloads();
    let mut st = state(LibraryPlace::Downloads);
    shown(&i, &mut st);
    st.click("active", "a", false, false);
    st.info = Some("a".into());
    st.select(LibraryPlace::Sharing);
    assert!(st.selection.is_empty() && st.anchor.is_none() && st.info.is_none());
}

#[test]
fn a_context_menu_acts_on_the_selection_it_is_in_or_on_its_row() {
    let (core, _d) = mock();
    let lib = library(&core);
    lib.set_open(true);
    let m = lib.model(NOW_MS);
    let rows: Vec<String> = m.list.as_ref().unwrap().groups.iter().flat_map(|g| g.rows.iter().map(|r| r.id.clone())).take(3).collect();
    assert_eq!(rows.len(), 3, "the mock downloads");
    lib.set_selection(vec![rows[0].clone(), rows[1].clone()]);
    lib.model(NOW_MS);
    let ids = |v: Vec<TransferRow>| v.into_iter().map(|r| r.id).collect::<Vec<_>>();
    assert_eq!(ids(lib.menu_rows(rows[1].clone())), [rows[0].clone(), rows[1].clone()]);
    assert_eq!(ids(lib.menu_rows(rows[2].clone())), [rows[2].clone()]);
    lib.set_selection(vec![rows[1].clone()]);
    assert_eq!(ids(lib.menu_rows(rows[1].clone())), [rows[1].clone()]);
}

// MARK: The Remove… question

#[test]
fn the_question_waits_under_its_row_or_in_the_title_bar_for_several() {
    let i = downloads();
    let mut st = state(LibraryPlace::Downloads);
    st.removal = Some(removal_question(vec![i.find("q").cloned().unwrap()], false));
    let m = shown(&i, &mut st);
    let p = m.removal.clone().expect("asked");
    assert_eq!((p.group_key.as_deref(), p.row_id.as_deref()), (Some("next"), Some("q")));
    assert_eq!(p.question.message, "Remove “q”? Files are kept.");
    st.removal = Some(removal_question(vec![i.find("a").cloned().unwrap(), i.find("r").cloned().unwrap()], true));
    let m2 = shown(&i, &mut st);
    let p = m2.removal.expect("asked");
    assert!(p.question.plural && p.group_key.is_none() && p.row_id.is_none());
    assert_ne!(m.signature, m2.signature, "where it is drawn is structure");
}

#[test]
fn the_question_outlives_a_refresh_and_goes_with_its_rows() {
    let i = downloads();
    let mut st = state(LibraryPlace::Downloads);
    st.removal = Some(removal_question(vec![i.find("a").cloned().unwrap()], false));
    assert!(shown(&i, &mut st).removal.is_some());
    assert!(shown(&i, &mut st).removal.is_some(), "the same rows again (a poll while the app was away)");
    let mut gone = downloads();
    gone.snapshot.downloading.remove(0);
    assert!(shown(&gone, &mut st).removal.is_none(), "its row left the screen");
    assert!(st.removal.is_none() && shown(&i, &mut st).removal.is_none(), "and it does not come back with it");
    // Several: it stays while any of them is on screen.
    st.removal = Some(removal_question(vec![i.find("a").cloned().unwrap(), i.find("b").cloned().unwrap()], true));
    assert!(shown(&gone, &mut st).removal.is_some());
}

#[test]
fn a_question_whose_row_is_folded_away_is_cancelled() {
    // Before, an invisible question could still be answered with Return: never remove what the user cannot see.
    let i = downloads();
    let mut st = state(LibraryPlace::Downloads);
    st.removal = Some(removal_question(vec![i.find("q").cloned().unwrap()], false));
    shown(&i, &mut st);
    st.folded.insert("downloads.next".into());
    assert!(shown(&i, &mut st).removal.is_none());
    st.folded.clear();
    assert!(shown(&i, &mut st).removal.is_none(), "unfolding does not bring it back");
    let (k, remove) = press(&mut st, &i, LibraryKey::Return, NONE, KeyScope::Window, &ctx());
    assert!(!k.handled && remove.is_none());
}

#[test]
fn another_place_cancels_the_question_and_the_same_one_keeps_it() {
    let i = downloads();
    let mut st = state(LibraryPlace::Downloads);
    st.removal = Some(removal_question(vec![i.find("a").cloned().unwrap()], false));
    st.select(LibraryPlace::Downloads);
    assert!(st.removal.is_some());
    st.select(LibraryPlace::Sharing);
    assert!(st.removal.is_none());
}

#[test]
fn return_answers_the_question_only_while_its_rows_are_on_screen() {
    let i = downloads();
    let mut st = state(LibraryPlace::Downloads);
    shown(&i, &mut st);
    let (k, remove) = press(&mut st, &i, LibraryKey::Return, NONE, KeyScope::Window, &ctx());
    assert!(!k.handled && remove.is_none(), "nothing asked");
    st.click("active", "a", false, false);
    st.removal = Some(removal_question(vec![i.find("a").cloned().unwrap()], true));
    let (k, _) = press(&mut st, &i, LibraryKey::Return, SHIFT, KeyScope::Window, &ctx());
    assert!(!k.handled && st.removal.is_some(), "⇧Return is not the answer");
    let (k, remove) = press(&mut st, &i, LibraryKey::Return, NONE, KeyScope::Window, &ctx());
    assert!(k.handled && st.removal.is_none() && st.selection.is_empty());
    let q = remove.expect("removes");
    assert_eq!((q.ids.as_slice(), q.delete_files), (&["a".to_string()][..], true));
    // Its row is not listed: Return drops the question instead and passes the key on.
    st.removal = Some(removal_question(vec![named("z", seed())], false));
    let (k, remove) = press(&mut st, &i, LibraryKey::Return, NONE, KeyScope::Window, &ctx());
    assert!(!k.handled && remove.is_none() && st.removal.is_none());
}

#[test]
fn escape_drops_the_question_before_the_selection() {
    let i = downloads();
    let mut st = state(LibraryPlace::Downloads);
    shown(&i, &mut st);
    st.click("active", "a", false, false);
    st.removal = Some(removal_question(vec![i.find("a").cloned().unwrap()], false));
    let (k, _) = press(&mut st, &i, LibraryKey::Escape, NONE, KeyScope::Window, &ctx());
    assert!(k.handled && st.removal.is_none() && st.selection.len() == 1);
    let (k, _) = press(&mut st, &i, LibraryKey::Escape, NONE, KeyScope::Window, &ctx());
    assert!(!k.handled, "then the list has it");
    let (k, _) = press(&mut st, &i, LibraryKey::Escape, NONE, KeyScope::List, &ctx());
    assert_eq!(k, KeyOutcome { handled: true, action: Some(LibraryKeyAction::ClearFileSelection) });
    assert!(st.selection.is_empty());
}

#[test]
fn the_buttons_cancel_and_remove() {
    let (core, _d) = mock();
    let lib = library(&core);
    lib.set_open(true);
    let m = lib.model(NOW_MS);
    let first = m.list.as_ref().unwrap().groups.iter().flat_map(|g| &g.rows).next().unwrap().transfer.clone();
    lib.ask_removal(vec![first.clone()], false);
    assert!(lib.model(NOW_MS).removal.is_some());
    lib.cancel_removal();
    assert!(lib.model(NOW_MS).removal.is_none());
    lib.ask_removal(vec![first.clone()], false);
    lib.confirm_removal();
    let m = lib.model(NOW_MS);
    assert!(m.removal.is_none());
    assert!(!m.list.unwrap().groups.iter().flat_map(|g| &g.rows).any(|r| r.id == first.id), "removed");
}

// MARK: Keys

#[test]
fn delete_asks_about_the_selected_rows_on_screen() {
    let i = downloads();
    let mut st = state(LibraryPlace::Downloads);
    shown(&i, &mut st);
    let (k, _) = press(&mut st, &i, LibraryKey::Delete, NONE, KeyScope::Window, &ctx());
    assert!(!k.handled, "nothing selected");
    st.click("recent", "r", false, false);
    st.click("active", "a", true, false);
    let (k, _) = press(&mut st, &i, LibraryKey::Delete, COMMAND, KeyScope::Window, &ctx());
    assert!(!k.handled && st.removal.is_none(), "⌘Delete is not Remove…");
    let files = WindowContext { file_torrent: Some("a".into()), file_rows: 2, ..ctx() };
    let (k, _) = press(&mut st, &i, LibraryKey::Delete, NONE, KeyScope::Window, &files);
    assert!(!k.handled && st.removal.is_none(), "not on selected files");
    let (k, _) = press(&mut st, &i, LibraryKey::Delete, SHIFT, KeyScope::Window, &ctx());
    assert!(k.handled);
    let q = st.removal.clone().unwrap();
    assert_eq!((q.ids.as_slice(), q.delete_files), (&["a".to_string(), "r".to_string()][..], true), "in list order, with their files");
    let (k, _) = press(&mut st, &i, LibraryKey::Delete, NONE, KeyScope::Window, &ctx());
    assert!(!k.handled && st.removal.as_ref().is_some_and(|x| x.delete_files), "not while it is open");
}

#[test]
fn keys_do_nothing_in_settings() {
    let i = downloads();
    let mut st = state(LibraryPlace::Downloads);
    shown(&i, &mut st);
    st.removal = Some(removal_question(vec![i.find("a").cloned().unwrap()], false));
    st.place = LibraryPlace::Settings;
    for key in [LibraryKey::Delete, LibraryKey::Return, LibraryKey::Escape] {
        assert!(!press(&mut st, &i, key, NONE, KeyScope::Window, &ctx()).0.handled);
    }
}

#[test]
fn space_toggles_the_selected_files_or_opens_get_info_of_one_row() {
    let i = downloads();
    let mut st = state(LibraryPlace::Downloads);
    shown(&i, &mut st);
    assert!(!press(&mut st, &i, LibraryKey::Space, NONE, KeyScope::List, &ctx()).0.handled);
    st.click("active", "a", false, false);
    let files = WindowContext { file_torrent: Some("a".into()), file_rows: 1, ..ctx() };
    assert_eq!(press(&mut st, &i, LibraryKey::Space, NONE, KeyScope::List, &files).0.action, Some(LibraryKeyAction::ToggleFileRows));
    assert!(st.info.is_none());
    let (k, _) = press(&mut st, &i, LibraryKey::Space, NONE, KeyScope::List, &ctx());
    assert!(k.handled && k.action.is_none() && st.info.as_deref() == Some("a"));
    st.info = None;
    st.click("active", "b", true, false);
    assert!(!press(&mut st, &i, LibraryKey::Space, NONE, KeyScope::List, &ctx()).0.handled, "two rows: no Get Info");
}

#[test]
fn command_i_opens_get_info_of_the_one_selected_row() {
    // SPEC 8.3 and the rows' context menus say ⌘I; the window did not handle it.
    let i = downloads();
    let mut st = state(LibraryPlace::Downloads);
    shown(&i, &mut st);
    assert!(!press(&mut st, &i, LibraryKey::I, COMMAND, KeyScope::Window, &ctx()).0.handled, "nothing selected");
    st.click("active", "a", false, false);
    assert!(!press(&mut st, &i, LibraryKey::I, NONE, KeyScope::Window, &ctx()).0.handled, "a plain i is not Get Info");
    assert!(!press(&mut st, &i, LibraryKey::I, KeyModifiers { shift: true, ..COMMAND }, KeyScope::Window, &ctx()).0.handled);
    let (k, _) = press(&mut st, &i, LibraryKey::I, COMMAND, KeyScope::Window, &ctx());
    assert!(k.handled && k.action.is_none() && st.info.as_deref() == Some("a"));
    st.info = None;
    st.click("active", "b", true, false);
    assert!(!press(&mut st, &i, LibraryKey::I, COMMAND, KeyScope::Window, &ctx()).0.handled, "two rows: no Get Info");
    st.place = LibraryPlace::Settings;
    st.selection = set(&["a"]);
    assert!(!press(&mut st, &i, LibraryKey::I, COMMAND, KeyScope::Window, &ctx()).0.handled, "not in Settings");
}

#[test]
fn the_arrows_open_and_close_file_lists_and_folders() {
    let mut i = downloads();
    i.snapshot.downloading[0].file_count = 3;
    let mut st = state(LibraryPlace::Downloads);
    shown(&i, &mut st);
    let right = |st: &mut LibraryState, c: &WindowContext| press(st, &i, LibraryKey::Right, NONE, KeyScope::List, c).0;
    let left = |st: &mut LibraryState, c: &WindowContext| press(st, &i, LibraryKey::Left, NONE, KeyScope::List, c).0;
    assert!(!right(&mut st, &ctx()).handled);
    st.click("active", "a", false, false);
    assert_eq!(right(&mut st, &ctx()).action, Some(LibraryKeyAction::OpenFiles { torrent_id: "a".into() }));
    assert!(!left(&mut st, &ctx()).handled, "its list is not open");
    let open = WindowContext { open_lists: vec!["a".into()], ..ctx() };
    assert!(!right(&mut st, &open).handled, "open already");
    assert_eq!(left(&mut st, &open).action, Some(LibraryKeyAction::CloseFiles { torrent_id: "a".into() }));
    st.click("active", "b", false, false);
    assert!(!right(&mut st, &ctx()).handled, "one file: no list");
    // In a file list: one selected file row opens or closes; several do nothing with ← and fall back to the row with →.
    let one = WindowContext { file_torrent: Some("a".into()), file_rows: 1, ..open.clone() };
    assert_eq!(right(&mut st, &one).action, Some(LibraryKeyAction::OpenFileFolder));
    assert_eq!(left(&mut st, &one).action, Some(LibraryKeyAction::CloseFileFolder));
    let two = WindowContext { file_rows: 2, ..one };
    assert!(!left(&mut st, &two).handled);
    st.click("active", "a", false, false);
    assert!(!right(&mut st, &two).handled, "its list is open");
}

#[test]
fn command_a_selects_every_row_on_screen() {
    let i = downloads();
    let mut st = state(LibraryPlace::Downloads);
    shown(&i, &mut st);
    let a = |st: &mut LibraryState, mods: KeyModifiers, scope: KeyScope, c: &WindowContext| press(st, &i, LibraryKey::A, mods, scope, c).0;
    assert!(!a(&mut st, NONE, KeyScope::Window, &ctx()).handled, "a plain a is not Select All");
    assert!(!a(&mut st, KeyModifiers { shift: true, ..COMMAND }, KeyScope::Window, &ctx()).handled);
    assert!(!a(&mut st, COMMAND, KeyScope::List, &ctx()).handled, "the window's key");
    assert!(st.selection.is_empty());
    let k = a(&mut st, COMMAND, KeyScope::Window, &ctx());
    assert!(k.handled && k.action.is_none());
    assert_eq!((st.selection.clone(), st.anchor.as_deref()), (set(&["a", "b", "q", "r"]), Some("a")));
    assert_eq!(shown(&i, &mut st).selection, ["a", "b", "q", "r"], "in list order, across groups");
    // A folded group's rows are not selected.
    st.selection.clear();
    st.folded.insert("downloads.next".into());
    shown(&i, &mut st);
    a(&mut st, COMMAND, KeyScope::Window, &ctx());
    assert_eq!(st.selection, set(&["a", "b", "r"]));
    // Selected file rows are deselected, as a click on a row does.
    let files = WindowContext { file_torrent: Some("a".into()), file_rows: 2, ..ctx() };
    assert_eq!(a(&mut st, COMMAND, KeyScope::Window, &files).action, Some(LibraryKeyAction::ClearFileSelection));
    // Nothing on screen, or in Settings: nothing to select (Edit › Select All goes on to whoever else wants it).
    let mut empty = state(LibraryPlace::Downloads);
    shown(&inputs(snap()), &mut empty);
    assert!(!press(&mut empty, &i, LibraryKey::A, COMMAND, KeyScope::Window, &ctx()).0.handled);
    st.place = LibraryPlace::Settings;
    st.selection.clear();
    assert!(!a(&mut st, COMMAND, KeyScope::Window, &ctx()).handled && st.selection.is_empty());
}

/// ↑ (`false`) or ↓ in the list; returns whether it was handled.
fn arrow(st: &mut LibraryState, i: &Inputs, down: bool, mods: KeyModifiers) -> bool {
    let key = if down { LibraryKey::Down } else { LibraryKey::Up };
    press(st, i, key, mods, KeyScope::List, &ctx()).0.handled
}

#[test]
fn the_arrows_move_the_selection_across_groups() {
    let i = downloads();
    let mut st = state(LibraryPlace::Downloads);
    shown(&i, &mut st);
    // Nothing selected: ↓ picks the first row, ↑ the last.
    assert!(arrow(&mut st, &i, true, NONE));
    assert_eq!((st.selection.clone(), st.anchor.as_deref()), (set(&["a"]), Some("a")));
    st.selection.clear();
    assert!(arrow(&mut st, &i, false, NONE));
    assert_eq!(st.selection, set(&["r"]));
    // One row at a time, from Downloading into Up Next and Recently Finished; at the end it stays.
    st.click("active", "a", false, false);
    let mut seen = Vec::new();
    for _ in 0..4 {
        assert!(arrow(&mut st, &i, true, NONE));
        seen.push(st.selection.iter().next().cloned().unwrap());
    }
    assert_eq!(seen, ["b", "q", "r", "r"]);
    assert!(arrow(&mut st, &i, false, NONE));
    assert_eq!((st.selection.clone(), st.anchor.as_deref()), (set(&["q"]), Some("q")), "a move is a plain click there");
    // Several selected: ↓ goes on after the last, ↑ before the first.
    st.selection = set(&["a", "q"]);
    arrow(&mut st, &i, true, NONE);
    assert_eq!(st.selection, set(&["r"]));
    st.selection = set(&["b", "r"]);
    arrow(&mut st, &i, false, NONE);
    assert_eq!(st.selection, set(&["a"]));
    // A folded group is skipped.
    st.folded.insert("downloads.next".into());
    shown(&i, &mut st);
    st.click("active", "b", false, false);
    arrow(&mut st, &i, true, NONE);
    assert_eq!(st.selection, set(&["r"]));
    // Not with ⌘, ⌥ or ⌃, not in the window's scope, not while file rows are selected, not on an empty list.
    for mods in [COMMAND, KeyModifiers { option: true, ..NONE }, KeyModifiers { control: true, ..NONE }] {
        assert!(!arrow(&mut st, &i, false, mods));
    }
    assert!(!press(&mut st, &i, LibraryKey::Down, NONE, KeyScope::Window, &ctx()).0.handled);
    let files = WindowContext { file_torrent: Some("a".into()), file_rows: 1, ..ctx() };
    assert!(!press(&mut st, &i, LibraryKey::Down, NONE, KeyScope::List, &files).0.handled);
    assert_eq!(st.selection, set(&["r"]));
    let mut empty = state(LibraryPlace::Downloads);
    shown(&inputs(snap()), &mut empty);
    assert!(!arrow(&mut empty, &i, true, NONE) && !arrow(&mut empty, &i, false, NONE));
}

#[test]
fn shift_arrows_extend_the_range_from_the_anchor() {
    let i = downloads();
    let mut st = state(LibraryPlace::Downloads);
    shown(&i, &mut st);
    st.click("active", "b", false, false);
    let mut seen = Vec::new();
    for down in [true, true, true, false, false, false] {
        assert!(arrow(&mut st, &i, down, SHIFT));
        seen.push(st.selection.iter().cloned().collect::<Vec<_>>().join(""));
    }
    assert_eq!(seen, ["bq", "bqr", "bqr", "bq", "b", "ab"], "it grows and shrinks at the moving end");
    assert_eq!(st.anchor.as_deref(), Some("b"));
    // The same range as a ⇧-click on the row moved to.
    let mut clicked = state(LibraryPlace::Downloads);
    shown(&i, &mut clicked);
    clicked.click("active", "b", false, false);
    clicked.click("active", "a", false, true);
    assert_eq!(clicked.selection, st.selection);
    // An anchor no longer on screen: the range starts at the selection's other end.
    st.selection = set(&["q"]);
    st.anchor = Some("gone".into());
    arrow(&mut st, &i, false, SHIFT);
    assert_eq!((st.selection.clone(), st.anchor.as_deref()), (set(&["b", "q"]), Some("q")));
    // Nothing selected: ⇧↓ picks the first row.
    st.selection.clear();
    arrow(&mut st, &i, true, SHIFT);
    assert_eq!(st.selection, set(&["a"]));
}

#[test]
fn get_info_follows_the_selection_the_arrows_move() {
    let i = downloads();
    let mut st = state(LibraryPlace::Downloads);
    shown(&i, &mut st);
    st.click("active", "a", false, false);
    st.info = Some("a".into());
    // One row selected: Get Info moves with it, across groups.
    let mut seen = Vec::new();
    for down in [true, true, false] {
        assert!(arrow(&mut st, &i, down, NONE));
        seen.push(st.info.clone().unwrap_or_default());
    }
    assert_eq!(seen, ["b", "q", "b"]);
    assert!(shown(&i, &mut st).list.unwrap().groups.iter().flat_map(|g| &g.rows).filter(|r| r.info_open).map(|r| r.id.as_str()).eq(["b"]));
    // ⇧ leaving several rows selected closes it; back to one row, it stays closed (nothing was open to follow).
    assert!(arrow(&mut st, &i, true, SHIFT));
    assert_eq!((st.selection.clone(), st.info.as_deref()), (set(&["b", "q"]), None));
    arrow(&mut st, &i, false, SHIFT);
    assert_eq!((st.selection.clone(), st.info.as_deref()), (set(&["b"]), None));
    // ⇧ shrinking a range back to one row while Get Info is open (opened from its ⓘ on a row of the range).
    st.selection = set(&["a", "b"]);
    st.anchor = Some("a".into());
    st.info = Some("b".into());
    arrow(&mut st, &i, false, SHIFT);
    assert_eq!((st.selection.clone(), st.info.as_deref()), (set(&["a"]), Some("a")));
    // Closed, it stays closed.
    st.info = None;
    arrow(&mut st, &i, true, NONE);
    assert_eq!((st.selection.clone(), st.info.as_deref()), (set(&["b"]), None));
    // At the end of the list the selection does not move, and neither does Get Info (its ⓘ opened it on another row).
    st.click("recent", "r", false, false);
    st.info = Some("q".into());
    arrow(&mut st, &i, true, NONE);
    assert_eq!((st.selection.clone(), st.info.as_deref()), (set(&["r"]), Some("q")));
    // A click on another row leaves Get Info to the host (its popover closes when the click lands outside it).
    st.click("active", "a", false, false);
    assert_eq!(st.info.as_deref(), Some("q"));
}

#[test]
fn the_row_an_arrow_moves_to_comes_into_view_at_once() {
    let (core, _d) = mock();
    let lib = library(&core);
    lib.set_open(true);
    let first = lib.model(NOW_MS).list.unwrap().groups[0].rows[0].id.clone();
    assert!(lib.key(LibraryKey::Down, NONE, KeyScope::List, ctx()).handled);
    let m = lib.model(NOW_MS);
    assert_eq!(m.selection, std::slice::from_ref(&first));
    let t = m.scroll_to.expect("a scroll");
    assert_eq!((t.row_id.as_str(), t.center), (first.as_str(), false), "only as far as it takes");
    lib.scroll_done(t.request);
    assert!(lib.model(NOW_MS).scroll_to.is_none());
    // A new download is centred.
    lib.new_download(first, ctx());
    assert!(lib.model(NOW_MS).scroll_to.expect("a scroll").center);
}

// MARK: Get Info

#[test]
fn get_info_stays_on_a_trackers_tab_for_its_torrents_folded_or_not() {
    let mut s = snap();
    s.trackers.push(tracker("t1", 9, false));
    s.downloading = vec![t1("d", download(TransferState::Downloading))];
    let rows = sharing(&[(SeedGroup::Dormant, vec![t1("x", seeding("x", SeedGroup::Dormant))])]);
    let mut i = Inputs { sharing: Some(SharingRows { rows: Vec::new(), ..rows }), ..inputs(s) };
    // The seed's group is folded: it is not listed, but it is the tracker's (its tab read all of them).
    i.tracker_seeds = vec![t1("x", seeding("x", SeedGroup::Dormant))];
    let mut st = state(LibraryPlace::Tracker { tracker_id: "t1".into() });
    st.folded.insert("tracker.dormant".into());
    assert_eq!(st.info_place(&i, "x"), st.place);
    assert_eq!(st.info_place(&i, "d"), st.place);
    // Another tracker's torrent is not this tab's.
    i.snapshot.queued = vec![on(named("o", download(TransferState::Queued)), "t2")];
    assert_eq!(st.info_place(&i, "o"), LibraryPlace::Downloads);
}

#[test]
fn get_info_goes_where_the_torrent_is_listed() {
    let i = downloads();
    let mut st = state(LibraryPlace::Sharing);
    assert_eq!(st.info_place(&i, "a"), LibraryPlace::Downloads, "still downloading");
    assert_eq!(st.info_place(&i, "q"), LibraryPlace::Downloads);
    assert_eq!(st.info_place(&i, "r"), LibraryPlace::Sharing, "just finished: listed in both, so it stays");
    assert_eq!(st.info_place(&i, "seed"), LibraryPlace::Sharing);
    st.place = LibraryPlace::Downloads;
    assert_eq!(st.info_place(&i, "r"), LibraryPlace::Downloads);
    st.place = LibraryPlace::FolderSync;
    assert_eq!(st.info_place(&i, "r"), LibraryPlace::Downloads);
}

#[test]
fn reveal_selects_the_row_where_its_get_info_is() {
    let (core, _d) = mock();
    let lib = library(&core);
    lib.set_open(true);
    lib.select_place(LibraryPlace::Sharing);
    lib.model(NOW_MS);
    let id = core.snapshot().downloading[0].id.clone();
    lib.reveal_row(id.clone());
    let m = lib.model(NOW_MS);
    assert_eq!((m.place, m.selection), (LibraryPlace::Downloads, vec![id.clone()]));
    lib.set_info(Some(id.clone()));
    lib.info_closed("other".into());
    assert!(lib.model(NOW_MS).list.unwrap().groups.iter().flat_map(|g| &g.rows).any(|r| r.id == id && r.info_open), "another popover's dismissal leaves it");
    lib.info_closed(id.clone());
    assert!(!lib.model(NOW_MS).list.unwrap().groups.iter().flat_map(|g| &g.rows).any(|r| r.info_open));
}

// MARK: A new download

fn open_state(place: LibraryPlace) -> LibraryState {
    let mut st = state(place);
    st.open = true;
    st
}

#[test]
fn a_new_download_takes_the_window_to_downloads_and_selects_it() {
    let i = downloads();
    let mut st = open_state(LibraryPlace::Sharing);
    st.selection = set(&["done"]);
    st.info = Some("done".into());
    st.search = "x".into();
    assert_eq!(st.new_download(&i.snapshot, "a", &ctx()), NewDownloadOutcome::Shown);
    assert_eq!(st.place, LibraryPlace::Downloads);
    assert_eq!((st.selection.clone(), st.anchor.as_deref(), st.info.as_deref()), (set(&["a"]), Some("a"), None));
    assert!(st.search.is_empty() && !st.downloads_flag);
    let m = shown(&i, &mut st);
    assert_eq!(m.scroll_to.as_ref().map(|s| s.row_id.as_str()), Some("a"));
    assert_eq!(m.selection, ["a"]);
}

#[test]
fn a_new_download_takes_a_trackers_tab_and_its_info_page_too() {
    let i = downloads();
    let mut st = open_state(LibraryPlace::Tracker { tracker_id: "public".into() });
    st.tracker_info = true;
    assert_eq!(st.new_download(&i.snapshot, "a", &ctx()), NewDownloadOutcome::Shown);
    assert!(st.place == LibraryPlace::Downloads && !st.tracker_info);
    // On Downloads already: it stays, Get Info closes.
    st.info = Some("b".into());
    assert_eq!(st.new_download(&i.snapshot, "b", &ctx()), NewDownloadOutcome::Shown);
    assert!(st.info.is_none() && st.selection == set(&["b"]));
}

#[test]
fn a_busy_user_only_gets_a_flag() {
    let i = downloads();
    let mut st = open_state(LibraryPlace::Settings);
    assert_eq!(st.new_download(&i.snapshot, "a", &ctx()), NewDownloadOutcome::Flagged);
    assert!(st.place == LibraryPlace::Settings && st.downloads_flag && st.selection.is_empty());
    assert!(shown(&i, &mut st).sidebar.places[0].flag.is_some());
    st.select(LibraryPlace::Downloads);
    assert!(!st.downloads_flag, "going there clears it");
    // An edit of the host's (a favorite's rename), on a tracker's Info page.
    let mut st = open_state(LibraryPlace::Tracker { tracker_id: "public".into() });
    st.tracker_info = true;
    let editing = WindowContext { editing: true, ..ctx() };
    assert_eq!(st.new_download(&i.snapshot, "a", &editing), NewDownloadOutcome::Flagged);
    assert!(st.tracker_info && st.downloads_flag);
    // The Remove… question.
    let mut st = open_state(LibraryPlace::Sharing);
    st.removal = Some(removal_question(vec![named("r", seed())], false));
    assert_eq!(st.new_download(&i.snapshot, "a", &ctx()), NewDownloadOutcome::Flagged);
    assert!(st.removal.is_some() && st.place == LibraryPlace::Sharing);
}

#[test]
fn a_busy_user_on_downloads_gets_no_dot_there() {
    // The dot goes only by going to Downloads (a click on the item already shown selects nothing new): on Downloads
    // it would stay until the user left and came back. The new row is listed right there anyway.
    let i = downloads();
    let mut st = open_state(LibraryPlace::Downloads);
    st.removal = Some(removal_question(vec![named("r", seed())], false));
    assert_eq!(st.new_download(&i.snapshot, "a", &ctx()), NewDownloadOutcome::Flagged);
    assert!(!st.downloads_flag && st.removal.is_some() && st.selection.is_empty());
    assert!(shown(&i, &mut st).sidebar.places[0].flag.is_none());
    let mut st = open_state(LibraryPlace::Downloads);
    st.add_sheet = true;
    let editing = WindowContext { editing: true, ..ctx() };
    assert_eq!(st.new_download(&i.snapshot, "a", &editing), NewDownloadOutcome::Flagged);
    assert!(!st.downloads_flag && st.add_sheet);
}

#[test]
fn a_new_download_leaves_the_add_sheet_open() {
    let (core, _d) = mock();
    let lib = library(&core);
    lib.set_open(true);
    lib.select_place(LibraryPlace::Sharing);
    lib.model(NOW_MS);
    lib.show_add_sheet();
    let id = core.snapshot().downloading[0].id.clone();
    assert_eq!(lib.new_download(id.clone(), ctx()), NewDownloadOutcome::Flagged);
    let m = lib.model(NOW_MS);
    assert!(m.add_sheet.is_some(), "the sheet stays, with what was typed in it");
    assert_eq!((m.place, m.selection.len(), m.scroll_to), (LibraryPlace::Sharing, 0, None));
    assert!(m.sidebar.places[0].flag.is_some(), "Downloads carries the dot");
    // The sheet closed: the next one takes the window again.
    lib.close_add_sheet();
    assert_eq!(lib.new_download(id.clone(), ctx()), NewDownloadOutcome::Shown);
    let m = lib.model(NOW_MS);
    assert!(m.place == LibraryPlace::Downloads && m.selection == [id] && m.sidebar.places[0].flag.is_none());
}

#[test]
fn a_click_in_the_notifier_takes_the_window_from_settings_not_from_the_add_sheet() {
    let i = downloads();
    let mut st = open_state(LibraryPlace::Settings);
    assert_eq!(st.show_torrent(&i.snapshot, "a", &ctx()), NewDownloadOutcome::Shown);
    assert!(st.place == LibraryPlace::Downloads && st.selection == set(&["a"]) && !st.downloads_flag);
    // The sheet stays, and so does the place behind it (Settings too): Downloads gets its dot, the row is not shown.
    for place in [LibraryPlace::Sharing, LibraryPlace::Settings] {
        let mut st = open_state(place.clone());
        st.add_sheet = true;
        st.add_text = "magnet:?xt".into();
        assert_eq!(st.show_torrent(&i.snapshot, "a", &ctx()), NewDownloadOutcome::Flagged);
        assert!(st.add_sheet && st.add_text == "magnet:?xt" && st.place == place && st.downloads_flag && st.selection.is_empty());
    }
    // The user's own edit or question still holds it.
    let mut st = open_state(LibraryPlace::Sharing);
    st.removal = Some(removal_question(vec![named("r", seed())], false));
    assert_eq!(st.show_torrent(&i.snapshot, "a", &ctx()), NewDownloadOutcome::Flagged);
    let editing = WindowContext { editing: true, ..ctx() };
    let mut st = open_state(LibraryPlace::Sharing);
    assert_eq!(st.show_torrent(&i.snapshot, "a", &editing), NewDownloadOutcome::Flagged);
}

#[test]
fn the_add_sheet_keeps_its_text_through_a_click_in_the_notifier() {
    let (core, _d) = mock();
    let lib = library(&core);
    lib.set_open(true);
    lib.select_place(LibraryPlace::Sharing);
    lib.model(NOW_MS);
    lib.show_add_sheet();
    lib.set_add_sheet_text("magnet:?xt=urn:btih:abc".into());
    let id = core.snapshot().downloading[0].id.clone();
    assert_eq!(lib.show_torrent(id.clone(), ctx()), NewDownloadOutcome::Flagged);
    let m = lib.model(NOW_MS);
    assert_eq!(m.add_sheet.map(|s| s.text).as_deref(), Some("magnet:?xt=urn:btih:abc"));
    assert_eq!((m.place, m.selection.len(), m.scroll_to), (LibraryPlace::Sharing, 0, None));
    assert!(m.sidebar.places[0].flag.is_some(), "Downloads carries the dot");
    // Closed, the text goes; the next click takes the window.
    lib.close_add_sheet();
    lib.show_add_sheet();
    assert_eq!(lib.model(NOW_MS).add_sheet.map(|s| s.text).as_deref(), Some(""));
    lib.close_add_sheet();
    assert_eq!(lib.show_torrent(id.clone(), ctx()), NewDownloadOutcome::Shown);
    let m = lib.model(NOW_MS);
    assert!(m.place == LibraryPlace::Downloads && m.selection == [id] && m.sidebar.places[0].flag.is_none());
}

#[test]
fn the_add_sheet_text_lives_while_the_sheet_is_open() {
    let (core, _d) = mock();
    let lib = library(&core);
    lib.set_open(true);
    lib.set_add_sheet_text("ignored".into());
    lib.show_add_sheet();
    assert_eq!(lib.model(NOW_MS).add_sheet.map(|s| s.text).as_deref(), Some(""), "typed nowhere: the sheet was closed");
    lib.set_add_sheet_text("https://x.org/a.torrent".into());
    lib.show_add_sheet();
    assert_eq!(lib.model(NOW_MS).add_sheet.map(|s| s.text).as_deref(), Some("https://x.org/a.torrent"), "open already: kept");
    lib.set_open(false);
    lib.set_open(true);
    assert!(lib.model(NOW_MS).add_sheet.is_none(), "a closed window closes it");
    lib.show_add_sheet();
    assert_eq!(lib.model(NOW_MS).add_sheet.map(|s| s.text).as_deref(), Some(""));
}

#[test]
fn ticking_files_of_an_open_list_does_not_take_the_window() {
    let mut s = downloads().snapshot;
    s.trackers.push(tracker("t1", 1, false));
    s.queued.push(t1("zz", download(TransferState::Queued)));
    let open = WindowContext { open_lists: vec!["zz".into()], ..ctx() };
    let mut st = open_state(LibraryPlace::Downloads);
    assert_eq!(st.new_download(&s, "zz", &ctx()), NewDownloadOutcome::Shown, "its list closed: the normal takeover");
    let mut st = open_state(LibraryPlace::Downloads);
    assert_eq!(st.new_download(&s, "zz", &open), NewDownloadOutcome::Unchanged);
    assert!(st.selection.is_empty() && st.scroll.is_none());
    let mut st = open_state(LibraryPlace::Tracker { tracker_id: "t1".into() });
    assert_eq!(st.new_download(&s, "zz", &open), NewDownloadOutcome::Unchanged, "listed on its own tracker's tab");
    st.tracker_info = true;
    assert_eq!(st.new_download(&s, "zz", &open), NewDownloadOutcome::Shown, "not on its Info page");
    let mut st = open_state(LibraryPlace::Tracker { tracker_id: "public".into() });
    assert_eq!(st.new_download(&s, "zz", &open), NewDownloadOutcome::Shown, "not that tracker's");
    let mut st = open_state(LibraryPlace::Sharing);
    assert_eq!(st.new_download(&s, "zz", &open), NewDownloadOutcome::Shown, "it leaves Sharing: follow it");
}

#[test]
fn each_new_download_asks_for_its_own_scroll() {
    let (core, _d) = mock();
    let lib = library(&core);
    lib.set_open(true);
    let id = core.snapshot().downloading[0].id.clone();
    assert_eq!(lib.new_download(id.clone(), ctx()), NewDownloadOutcome::Shown);
    let first = lib.model(NOW_MS).scroll_to.expect("a scroll");
    lib.new_download(id.clone(), ctx());
    let second = lib.model(NOW_MS).scroll_to.expect("a scroll");
    assert_ne!(first, second);
    lib.scroll_done(first.request);
    assert!(lib.model(NOW_MS).scroll_to.is_some(), "an old request does not end the new one");
    lib.scroll_done(second.request);
    assert!(lib.model(NOW_MS).scroll_to.is_none());
}

#[test]
fn a_closed_window_opens_on_the_download_that_arrived_meanwhile() {
    let (core, _d) = mock();
    let lib = library(&core);
    let s = core.snapshot();
    let (a, b) = (s.downloading[0].id.clone(), s.downloading[1].id.clone());
    assert_eq!(lib.new_download(a.clone(), ctx()), NewDownloadOutcome::Remembered);
    assert_eq!(lib.new_download(b.clone(), ctx()), NewDownloadOutcome::Remembered, "the newest wins");
    lib.set_open(true);
    assert_eq!(lib.take_arrival(true, ctx()), NewDownloadOutcome::Shown);
    let m = lib.model(NOW_MS);
    assert_eq!((m.place, m.selection), (LibraryPlace::Downloads, vec![b.clone()]));
    assert_eq!(lib.take_arrival(true, ctx()), NewDownloadOutcome::Unchanged, "used up");
    // Opened on a place of its own (a row's Get Info, Settings): forgotten, not shown.
    lib.set_open(false);
    lib.new_download(a.clone(), ctx());
    lib.set_open(true);
    assert_eq!(lib.take_arrival(false, ctx()), NewDownloadOutcome::Unchanged);
    assert_eq!(lib.take_arrival(true, ctx()), NewDownloadOutcome::Unchanged);
}

#[test]
fn a_download_that_is_gone_is_not_shown() {
    let i = downloads();
    let mut st = state(LibraryPlace::Downloads);
    assert_eq!(st.new_download(&i.snapshot, "gone", &ctx()), NewDownloadOutcome::Remembered);
    st.open = true;
    assert_eq!(st.take_arrival(&i.snapshot, true, &ctx()), NewDownloadOutcome::Unchanged);
    assert!(st.arrival.is_none() && st.selection.is_empty());
}

#[test]
fn closing_the_window_forgets_what_it_showed_but_not_its_folds() {
    let (core, _d) = mock();
    let lib = library(&core);
    lib.set_open(true);
    lib.set_visible(true);
    lib.toggle_fold("next".into());
    lib.select_place(LibraryPlace::Sharing);
    lib.set_search("a".into());
    lib.show_add_sheet();
    assert!(lib.model(NOW_MS).add_sheet.is_some());
    lib.set_open(false);
    let m = lib.model(NOW_MS);
    assert_eq!(m.place, LibraryPlace::Downloads);
    assert!(m.add_sheet.is_none() && m.refresh_ms.is_none());
    assert!(m.list.unwrap().groups.iter().any(|g| g.key == "next" && g.folded), "the folds stay");
}

// MARK: The add sheet

#[test]
fn the_add_sheet_takes_a_link_without_the_spaces_around_it() {
    assert_eq!(add_link_source("  magnet:?xt=urn:btih:abc \t".into()).as_deref(), Some("magnet:?xt=urn:btih:abc"));
    assert_eq!(add_link_source("\u{a0}https://x.org/a.torrent\u{3000}".into()).as_deref(), Some("https://x.org/a.torrent"));
    assert_eq!(add_link_source(" \t ".into()), None);
    assert_eq!(add_link_source(String::new()), None);
    // Line breaks are not trimmed (the field is one line; the core reads the link).
    assert_eq!(add_link_source("a\n".into()).as_deref(), Some("a\n"));
    let mut st = state(LibraryPlace::Downloads);
    st.add_sheet = true;
    st.add_text = "x".into();
    let sheet = add_sheet_of(&st).expect("open");
    assert_eq!(sheet.text, "x");
    assert_eq!((sheet.title.as_str(), sheet.placeholder.as_str(), sheet.confirm_title.as_str()), ("Add a Download", "Magnet link or URL", "Add"));
    assert_eq!((sheet.choose_file_title.as_str(), sheet.choose_file_help.as_str()), ("Choose .torrent File…", "Pick a .torrent file from disk instead"));
    assert_eq!((sheet.cancel_help.as_str(), sheet.confirm_help.as_str()), ("Close without adding", "Add this magnet link or URL as a download"));
}
