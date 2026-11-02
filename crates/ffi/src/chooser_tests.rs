//! The chooser (SPEC 7A.6): what starts ticked, the ticks, picks and counts, Start, Cancel and a category change on the
//! shared choosers, and the words of Cancel and Start.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use super::*;
use crate::types::{Category, ChoiceGroupInfo, ChoiceGroupKind, ChoiceInfo, ChoiceItemInfo};

pub(crate) fn file(label: &str, bytes: u64, idx: u32, selected: bool) -> ChoiceItemInfo {
    ChoiceItemInfo { label: label.into(), bytes, files: vec![idx], selected, in_library: false, other_quality: false, quality: None, this_mac: false }
}

/// A season of two episodes (ticked) and an Extras group with a sample (unticked, the engine's preselection).
pub(crate) fn info(id: &str) -> ChoiceInfo {
    ChoiceInfo {
        id: id.into(),
        name: "Show.S01".into(),
        display_name: "Show S01".into(),
        tracker: String::new(),
        awaiting: true,
        reasons: vec![],
        unsure: false,
        structure: "tv".into(),
        groups: vec![
            ChoiceGroupInfo { label: "Season 1".into(), kind: ChoiceGroupKind::Season, items: vec![file("S01E01", 400, 0, true), file("S01E02", 600, 1, true)] },
            ChoiceGroupInfo { label: "Extras".into(), kind: ChoiceGroupKind::Extras, items: vec![file("sample.mkv", 50, 2, false)] },
        ],
        file_count: 3,
        total_bytes: 1050,
        selected_bytes: 1000,
        free_bytes: Some(1_000_000),
        destination: Some("/Users/me/Media/TV".into()),
        category: Category::Tv,
        confidence: 0.9,
        library_checked: false,
        rule_name: String::new(),
        then_text: String::new(),
        summary_text: String::new(),
    }
}

/// A stand-in for the core: records what it was asked.
#[derive(Default)]
pub(crate) struct Fake {
    pub(crate) confirmed: Mutex<Vec<(String, Vec<bool>)>>,
    pub(crate) cancelled: Mutex<Vec<(String, bool)>>,
    pub(crate) loads: AtomicU32,
    pub(crate) fail: AtomicBool,
    pub(crate) categories: Mutex<Vec<Category>>,
    pub(crate) sources: Mutex<Vec<String>>,
    pub(crate) note: Mutex<Option<CancelNote>>,
    /// What "Choose Files…" answers (`None`: an error) and what the countdown calls were.
    pub(crate) choose: Mutex<Option<bool>>,
    pub(crate) pending: Mutex<Vec<String>>,
}

impl ChoiceSource for Fake {
    fn load(&self, id: &str, _reopen: bool) -> Result<ChoiceInfo, CoreError> {
        self.loads.fetch_add(1, Ordering::SeqCst);
        if self.fail.load(Ordering::SeqCst) {
            return Err(CoreError::NotFound { id: id.into() });
        }
        let mut i = info(id);
        if let Some(&c) = self.categories.lock().unwrap().last() {
            i.category = c;
            i.destination = Some(if c == Category::Tv { "/Users/me/Media/TV" } else { "/Users/me/Media/Other" }.into());
        }
        Ok(i)
    }
    fn confirm(&self, id: &str, files: &[bool]) -> Result<(), CoreError> {
        self.confirmed.lock().unwrap().push((id.into(), files.to_vec()));
        Ok(())
    }
    fn cancel(&self, id: &str, keep_file: bool) -> Option<CancelNote> {
        self.cancelled.lock().unwrap().push((id.into(), keep_file));
        self.note.lock().unwrap().clone()
    }
    fn has_source(&self, id: &str) -> bool {
        self.sources.lock().unwrap().iter().any(|s| s == id)
    }
    fn set_category(&self, _id: &str, category: Category) {
        self.categories.lock().unwrap().push(category);
    }
}

pub(crate) fn choices() -> (Arc<Choices>, Arc<Fake>) {
    let fake = Arc::new(Fake::default());
    (Choices::with(fake.clone(), Some("/Users/me".into())), fake)
}

fn chooser(info: &ChoiceInfo) -> Arc<Chooser> {
    Chooser::with(info, "", Some("/Users/me"))
}

/// `rest`, in the test's home folder (`/Users/me`), as the core writes it for the user: `~/rest` on Unix systems, the
/// full path on Windows, where `~` means nothing (`shown_in`, as `trs_platform::dirs::shown`).
pub(crate) fn at_home(rest: &str) -> String {
    if cfg!(unix) { format!("~/{rest}") } else { format!("/Users/me/{rest}") }
}

fn row<'a>(v: &'a ChooserPage, title: &str) -> &'a ChooserRow {
    v.rows.iter().find(|r| r.title == title).unwrap_or_else(|| panic!("no row {title}"))
}

#[test]
fn a_held_torrent_starts_with_everything_but_the_extras_in_one_shared_chooser() {
    let (store, fake) = choices();
    let c = store.chooser("t1".into(), false).unwrap();
    assert_eq!(c.file_flags(), [true, true, false], "Start without opening the list takes everything but the Extras");
    assert_eq!((c.selected_bytes(), c.total_bytes()), (1000, 1050));
    let v = c.view(false);
    assert_eq!(v.all.check, FileCheck::Mixed);
    assert_eq!(v.origin, "Choose what to download");
    assert!(Arc::ptr_eq(&store.chooser("t1".into(), false).unwrap(), &c), "one chooser per torrent");
    assert_eq!(fake.loads.load(Ordering::SeqCst), 1);
    assert_eq!(store.ids(), ["t1"]);
}

#[test]
fn select_all_none_and_the_toggle_between_them() {
    let (store, _) = choices();
    let c = store.chooser("t1".into(), false).unwrap();
    c.select_all(true);
    assert_eq!((c.file_flags(), c.view(false).all.check), (vec![true, true, true], FileCheck::On));
    assert_eq!(c.view(false).all.title, "Select none");
    c.toggle_all();
    let v = c.view(false);
    assert_eq!((c.file_flags(), v.all.check, v.can_confirm), (vec![false, false, false], FileCheck::Off, false));
    assert_eq!((v.all.title.as_str(), v.all.help.as_str()), ("Select all", "Tick every file"));
    c.toggle_all();
    assert_eq!(c.view(false).all.check, FileCheck::On);
    c.select_all(false);
    c.toggle("g0i1".into());
    assert_eq!(c.file_flags(), [false, true, false]);
    assert_eq!(row(&c.view(false), "Season 1").check, FileCheck::Mixed);
}

#[test]
fn start_sends_the_ticks_and_forgets_the_chooser() {
    let (store, fake) = choices();
    let c = store.chooser("t1".into(), false).unwrap();
    c.toggle("g0i0".into()); // unticks E01
    store.confirm("t1".into()).unwrap();
    assert_eq!(*fake.confirmed.lock().unwrap(), [("t1".to_string(), vec![false, true, false])]);
    assert!(store.loaded("t1".into()).is_none());
}

#[test]
fn cancel_removes_the_held_torrent_keeps_the_file_on_shift_and_passes_the_note_on() {
    let (store, fake) = choices();
    store.chooser("t1".into(), false);
    assert_eq!(store.cancel("t1".into(), false), None);
    assert!(store.loaded("t1".into()).is_none() && fake.confirmed.lock().unwrap().is_empty());
    let note = CancelNote { title: "Couldn't move a.torrent to the Trash".into(), body: "The torrent was removed.".into() };
    *fake.note.lock().unwrap() = Some(note.clone());
    assert_eq!(store.cancel("t2".into(), true), Some(note));
    assert_eq!(*fake.cancelled.lock().unwrap(), [("t1".to_string(), false), ("t2".to_string(), true)]);
}

#[test]
fn a_chooser_that_cannot_be_loaded_yet_is_none_and_prune_keeps_only_the_waiting() {
    let (store, fake) = choices();
    fake.fail.store(true, Ordering::SeqCst);
    assert!(store.chooser("t1".into(), false).is_none());
    fake.fail.store(false, Ordering::SeqCst);
    store.chooser("a".into(), false);
    store.chooser("b".into(), false);
    store.prune(vec!["b".into()]);
    assert_eq!(store.ids(), ["b"]);
    store.forget("b".into());
    assert!(store.ids().is_empty());
}

#[test]
fn folding_a_group_never_changes_the_ticks_and_a_leaf_has_nothing_to_fold() {
    let c = chooser(&info("t1"));
    let ticks = c.file_flags();
    assert_eq!(c.view(false).open_groups, ["g0"], "the first ticked group starts open");
    c.toggle_folded("g0".into());
    assert!(c.view(false).open_groups.is_empty());
    c.toggle_folded("g1".into());
    c.toggle_folded("g0".into());
    assert_eq!(c.view(false).open_groups, ["g1", "g0"]);
    assert_eq!(c.file_flags(), ticks);
    c.toggle_folded("g0i0".into());
    assert_eq!(c.view(false).open_groups, ["g1", "g0"], "a leaf has nothing to fold");
    let v = c.view(false);
    assert_eq!(v.rows.iter().map(|r| (r.title.as_str(), r.level)).collect::<Vec<_>>(), [
        ("Season 1", 0),
        ("S01E01", 1),
        ("S01E02", 1),
        ("Extras", 0),
        ("sample.mkv", 1)
    ]);
    let s = row(&v, "Season 1");
    assert!(s.foldable && s.open);
    assert_eq!((s.fold_help.as_str(), s.fold_label.as_str()), ("Hide the contents", "Collapse"));
    assert!(!row(&v, "S01E01").foldable && row(&v, "S01E01").fold_help.is_empty());
}

#[test]
fn deselecting_every_episode_then_picking_one_stays_usable() {
    // The engine's own preselection, without the held torrent's "everything".
    let c = chooser(&info("t1"));
    for id in ["g0i0", "g0i1"] {
        c.toggle(id.into());
    }
    let v = c.view(false);
    assert_eq!((v.checked_count, row(&v, "Season 1").check, v.all.check, v.can_confirm), (0, FileCheck::Off, FileCheck::Off, false));
    assert_eq!(v.buttons.start.as_ref().map(|s| (s.title.as_str(), s.enabled)), Some(("Start", false)), "Start is disabled while nothing is ticked");
    assert!(v.totals_line.starts_with("Nothing selected · "));
    c.toggle("g0i1".into());
    let v = c.view(false);
    assert!(v.can_confirm && v.buttons.start.unwrap().enabled);
    assert!(v.totals_line.starts_with("1 episode · "), "{}", v.totals_line);
    assert_eq!(c.file_flags(), [false, true, false]);
    // The season box and select all / none round trip from empty.
    c.select_all(false);
    c.toggle("g0".into());
    assert_eq!(row(&c.view(false), "Season 1").check, FileCheck::On);
    c.toggle("g0".into());
    assert!(!c.view(false).can_confirm);
}

#[test]
fn quick_picks_tick_what_they_say_and_a_tick_leaves_the_pick() {
    let c = chooser(&chooser_sample());
    let v = c.view(false);
    assert_eq!(v.picks.iter().map(|p| (p.title.as_str(), p.selected)).collect::<Vec<_>>(), [
        ("All", false),
        ("What I don't have", true),
        ("Latest season", false)
    ]);
    assert_eq!(v.picks[1].help, "Use the What I don't have preset for what to download");
    c.select_all(false);
    c.apply_pick(ChooserPick::All);
    let v = c.view(false);
    assert!(v.can_confirm && v.picks[0].selected);
    assert!(v.totals_line.starts_with("5 seasons · "), "{}", v.totals_line);
    assert_eq!(row(&v, "Extras").check, FileCheck::Off, "All leaves the Extras");
    c.apply_pick(ChooserPick::Latest);
    assert!(c.view(false).totals_line.starts_with("1 season · "));
    c.toggle("g4".into());
    assert!(c.view(false).picks.iter().all(|p| !p.selected));
    // One pick only: no segmented control.
    let mut one = info("t1");
    one.structure = "movies".into();
    assert!(chooser(&one).view(false).picks.is_empty());
}

#[test]
fn whole_seasons_and_single_episodes_are_counted_apart() {
    let c = chooser(&chooser_sample());
    c.select_all(false);
    c.toggle("g2".into());
    assert!(c.view(false).totals_line.starts_with("1 season · "));
    c.toggle("g3i0".into());
    assert!(c.view(false).totals_line.starts_with("1 season + 1 episode · "));
    c.toggle("g3i1".into());
    assert!(c.view(false).totals_line.starts_with("1 season + 2 episodes · "));
}

#[test]
fn the_sample_reads_as_the_chooser_window_shows_it() {
    let c = chooser(&chooser_sample());
    let v = c.view(true);
    assert_eq!(v.name, "Fallen.Kingdom.Complete.Series.S01-S05.1080p.WEB-DL.x264");
    assert_eq!(v.summary, "212 GB · 902 files · tracker-y.net");
    assert_eq!((v.category_title.as_str(), v.category_detail.as_str()), ("TV · Fallen Kingdom", "98% confident"));
    assert_eq!((v.change_title.as_str(), v.change_help.as_str()), ("Change", "Change what kind of content this is"));
    assert_eq!(v.categories.iter().filter(|c| c.checked).map(|c| c.title.as_str()).collect::<Vec<_>>(), ["TV"]);
    assert_eq!(v.categories.len(), 8);
    assert_eq!(v.rule_lines, [
        ChooserLine { label: "Rule ".into(), value: "Y – seed to 1.0".into(), emphasized: true },
        ChooserLine { label: "Saves to ".into(), value: "~/Media/TV/Fallen Kingdom".into(), emphasized: false },
        ChooserLine { label: "Then ".into(), value: "seeds until ratio 1.0".into(), emphasized: false },
    ]);
    assert_eq!(v.notice.as_deref(), Some("Seasons 1 and 2 are already in ~/Media/TV/Fallen Kingdom, so they are not selected."));
    let s1 = row(&v, "Season 1");
    assert_eq!((s1.detail.as_str(), s1.badge.as_deref(), s1.dimmed, s1.check), ("10 episodes", Some("In library"), true, FileCheck::Off));
    assert_eq!((s1.check_help.as_str(), s1.check_value.as_str()), ("Download this", "unchecked"));
    let s3 = row(&v, "Season 3");
    assert_eq!((s3.badge.as_deref(), s3.dimmed, s3.check, s3.check_help.as_str()), (None, false, FileCheck::On, "Skip this"));
    assert_eq!(row(&v, "S03E01").detail, "1080p WEB-DL");
    let x = row(&v, "Extras");
    assert_eq!((x.detail.as_str(), x.dimmed), ("samples, featurettes, .nfo", true));
    assert_eq!(v.totals_line, "3 seasons · 126 GB · 412 GB free");
    assert_eq!(v.selected_line, "126 GB of 212 GB");
    assert!(!v.not_enough_space);
    assert_eq!((v.space_warning.as_str(), v.space_note.as_str()), ("Not enough space", "Not enough space at the destination"));
    assert_eq!(v.open_groups, ["g2"]);
}

#[test]
fn the_place_the_count_and_the_space_follow_the_info() {
    let mut i = info("t1");
    i.free_bytes = Some(10);
    i.destination = None;
    i.tracker = "tracker-y.net".into();
    i.file_count = 1;
    let v = chooser(&i).view(false);
    assert!(v.not_enough_space);
    assert_eq!(v.rule_lines[0], ChooserLine { label: "Saves to ".into(), value: "the download folder".into(), emphasized: false });
    assert_eq!(v.summary, "1.1 KB · 1 file · tracker-y.net");
    assert!(chooser(&i).destination().is_none(), "no place of its own");
    assert_eq!(chooser(&info("t1")).destination(), Some(at_home("Media/TV")));
}

#[test]
fn a_build_for_this_computer_and_another_quality_say_so() {
    let mut i = info("t1");
    i.groups[0].items[0].this_mac = true;
    i.groups[0].items[0].quality = Some("arm64".into());
    i.groups[0].items[1].this_mac = true;
    i.groups[0].items[1].other_quality = true;
    let v = chooser(&i).view(false);
    assert_eq!(row(&v, "S01E01").detail, concat!("arm64 · for this ", computer!()));
    assert_eq!(row(&v, "S01E02").detail, concat!("for this ", computer!()));
    assert_eq!(row(&v, "S01E02").badge.as_deref(), Some("Other quality"));
}

#[test]
fn a_torrent_of_other_things_lists_its_items_plainly() {
    let mut i = info("t1");
    i.category = Category::Other;
    i.groups.push(ChoiceGroupInfo { label: "Other".into(), kind: ChoiceGroupKind::Other, items: vec![file("readme.txt", 1, 3, true)] });
    i.file_count = 4;
    let c = chooser(&i);
    let v = c.view(false);
    assert_eq!(v.rows.iter().map(|r| (r.title.as_str(), r.level)).collect::<Vec<_>>(), [
        ("S01E01", 0),
        ("S01E02", 0),
        ("Extras", 0),
        ("readme.txt", 0)
    ]);
    assert!(v.totals_line.starts_with("2 items · "), "the loose file is not counted: {}", v.totals_line);
}

#[test]
fn a_running_torrents_files_have_cancel_and_download() {
    let mut i = info("t1");
    i.awaiting = false;
    let v = chooser(&i).view(true);
    assert!(!v.awaiting);
    assert_eq!((v.buttons.cancel.title.as_str(), v.buttons.cancel.help.as_str()), ("Cancel", "Close without changing anything"));
    assert!(v.buttons.keep_file.is_none());
    let d = v.buttons.start.unwrap();
    assert_eq!((d.title.as_str(), d.help.as_str(), d.keys.as_slice()), ("Download", "Download the files ticked now", &[HeldKey::Return][..]));
}

#[test]
fn the_cursor_moves_with_the_arrows_and_space_ticks_its_row() {
    let c = chooser(&info("t1"));
    c.toggle_folded("g0".into()); // closed: Season 1, Extras; the click puts the cursor on the season
    assert_eq!(c.view(false).rows.iter().filter(|r| r.cursor).map(|r| r.title.as_str()).collect::<Vec<_>>(), ["Season 1"]);
    c.move_cursor(1);
    c.move_cursor(1);
    assert!(row(&c.view(false), "Extras").cursor, "it stops at the last row");
    c.toggle_cursor();
    assert_eq!(c.file_flags(), [true, true, true]);
    c.move_cursor(-5);
    c.toggle_cursor();
    assert_eq!(c.file_flags(), [false, false, true]);
    assert_eq!(c.find_row("s01e0".into()).as_deref(), Some("g0i0"));
    assert_eq!(c.find_row("extras".into()).as_deref(), Some("g1"));
}

#[test]
fn a_category_change_rebuilds_the_shared_chooser_and_keeps_the_users_ticks() {
    let (store, fake) = choices();
    let c = store.chooser("t1".into(), false).unwrap();
    assert_eq!((c.category(), c.destination()), (Category::Tv, Some(at_home("Media/TV"))));
    // Untouched: the new plan's default again.
    store.change_category("t1".into(), Category::Other);
    assert!(Arc::ptr_eq(&store.loaded("t1".into()).unwrap(), &c), "the same chooser, rebuilt in place");
    assert_eq!((c.category(), c.destination()), (Category::Other, Some(at_home("Media/Other"))));
    assert_eq!(c.file_flags(), [true, true, false]);
    // Changed ticks stay where the same files are still there.
    c.toggle("g0i0".into());
    let kept = c.file_flags();
    store.change_category("t1".into(), Category::Tv);
    assert_eq!((c.category(), c.file_flags()), (Category::Tv, kept));
    // The current category again: nothing.
    store.change_category("t1".into(), Category::Tv);
    assert_eq!(*fake.categories.lock().unwrap(), [Category::Other, Category::Tv]);
    // Not loaded: the engine is told all the same.
    store.change_category("t9".into(), Category::Music);
    assert_eq!(fake.categories.lock().unwrap().last(), Some(&Category::Music));
}

#[test]
fn select_none_is_an_edit_a_category_change_keeps() {
    let (store, _) = choices();
    let c = store.chooser("t1".into(), false).unwrap();
    c.select_all(false);
    store.change_category("t1".into(), Category::Other);
    assert_eq!(c.file_flags(), [false, false, false]);
}

#[test]
fn cancel_and_start_have_one_set_of_words_and_the_keys_go_to_the_active_block() {
    let a = held_actions(true, true, true, "Start downloading now".into());
    assert_eq!((a.cancel.title.as_str(), a.cancel.keys.as_slice()), ("Cancel", &[HeldKey::Escape][..]));
    assert_eq!(a.cancel.help, cancel_help(true));
    let keep = a.keep_file.unwrap();
    assert_eq!((keep.caption.as_str(), keep.keys.as_slice()), ("Keep .torrent file", &[HeldKey::ShiftEscape][..]));
    let s = a.start.unwrap();
    assert_eq!((s.title.as_str(), s.help.as_str(), s.enabled, s.keys.as_slice()), ("Start", "Start downloading now", true, &[HeldKey::Return][..]));
    let b = held_actions(false, false, false, String::new());
    assert!(b.keep_file.is_none(), "no file, no Shift variant");
    assert!(b.cancel.keys.is_empty() && b.start.as_ref().is_some_and(|s| s.keys.is_empty() && !s.enabled), "keys only for the active block");
    assert_eq!(held(false, true, None).start, None);
}

#[test]
fn every_category_is_offered_once_and_the_current_one_is_checked() {
    let all = category_choices(Category::Game);
    assert_eq!(all.iter().map(|c| c.title.as_str()).collect::<Vec<_>>(), ["Movie", "TV", "Music", "Audiobook", "Book", "App", "Game", "Other"]);
    assert_eq!(all.iter().filter(|c| c.checked).map(|c| c.category).collect::<Vec<_>>(), [Category::Game]);
}

#[test]
fn paths_in_the_home_folder_are_written_with_a_tilde() {
    if cfg!(unix) {
        assert_eq!(shown_in("/Users/me/Media/TV", Some("/Users/me")), "~/Media/TV");
        assert_eq!(shown_in("/Users/me", Some("/Users/me/")), "~");
        assert_eq!(shown_in("/Users/meg/x", Some("/Users/me")), "/Users/meg/x");
    } else {
        assert_eq!(shown_in(r"C:\Users\me\Media\TV", Some(r"C:\Users\me")), r"C:\Users\me\Media\TV", "Windows writes the full path");
    }
    assert_eq!(shown_in("/Volumes/Big", Some("/Users/me")), "/Volumes/Big");
    assert_eq!(shown_in("/x", None), "/x");
}
