//! The starting notifier (SPEC 7A.7, 8.6): which torrents it shows, each countdown, the choice blocks and the magnet
//! links fetching their files, when it shows, stays and closes, which block owns the keys, and its buttons.

use std::sync::atomic::Ordering;

use super::*;
use crate::chooser::tests::{at_home, choices, Fake};
use crate::chooser::HeldKey;
use crate::row_text::tests::row as plain_row;
use crate::types::{PendingStart, TransferState};

/// 2027-01-15, in ms.
const T0: u64 = 1_800_000_000_000;

impl PendingSource for Fake {
    fn choose(&self, id: &str) -> Result<bool, CoreError> {
        self.pending.lock().unwrap().push(format!("choose {id}"));
        self.choose.lock().unwrap().ok_or(CoreError::NotFound { id: id.into() })
    }
    fn pause(&self, id: &str, pause: bool) {
        self.pending.lock().unwrap().push(format!("{} {id}", if pause { "pause" } else { "resume" }));
    }
    fn start_now(&self, id: &str) {
        self.pending.lock().unwrap().push(format!("start {id}"));
    }
}

fn starting() -> (Arc<Starting>, Arc<Fake>) {
    let (c, fake) = choices();
    (Starting::with(c, fake.clone(), Some("/Users/me".into())), fake)
}

fn wait(starts_in_ms: u64) -> Option<PendingStart> {
    Some(PendingStart { starts_at_ms: T0 + starts_in_ms, remaining_ms: starts_in_ms, paused: false })
}

fn row(id: &str, pending: Option<PendingStart>, awaiting: bool) -> TransferRow {
    let mut r = plain_row(id, TransferState::Queued);
    r.display_name = if id == "b" { "Sintel (2010)".into() } else { "Night of the Living Dead (1968)".into() };
    r.category = Category::Movie;
    r.size_bytes = 1_000_000_000;
    r.save_path = format!("/Users/me/Media/Movies/{id}");
    r.pending = pending;
    r.awaiting_choice = awaiting;
    r
}

fn magnet(id: &str) -> TransferRow {
    let mut r = row(id, None, false);
    r.size_bytes = 0;
    r.category = Category::Other;
    r.save_path = "/Users/me/Downloads/abcdef".into();
    r
}

fn ids(p: &StartingPage) -> Vec<&str> {
    p.blocks.iter().map(|b| b.id.as_str()).collect()
}

fn countdown(b: &StartingBlock) -> StartingCountdown {
    match b.state {
        StartingBlockState::Countdown { countdown } => countdown,
        ref s => panic!("not a countdown: {s:?}"),
    }
}

#[test]
fn each_torrent_counts_down_to_its_own_deadline() {
    let (s, _) = starting();
    let p = s.refresh(vec![row("a", wait(10_000), false), row("b", wait(7_000), false), row("c", None, false)], T0, false);
    assert_eq!(ids(&p), ["a", "b"]);
    assert_eq!(starting_tick(countdown(&p.blocks[0]), T0 + 3_000).line, "Starting in 7 s");
    assert_eq!(starting_tick(countdown(&p.blocks[1]), T0 + 3_000).line, "Starting in 4 s", "each torrent counts to its own deadline");
    assert_eq!(p.blocks[0].line, "Starting in 10 s");
    assert_eq!(p.blocks[0].detail, format!("Movie · 1.0 GB · {}", at_home("Media/Movies")));
    assert_eq!(p.blocks[0].open_help, "Open in Seedwell");
    assert_eq!(p.show, StartingShow::Show { stays: false }, "a countdown alone can be clicked away");
    assert!(p.compact && p.close_help.is_none());
    assert_eq!(p.tick_ms, 1_000);
}

#[test]
fn the_line_counts_whole_seconds_down_and_only_says_zero_when_it_starts() {
    let c = StartingCountdown { starts_at_ms: T0 + 10_000, total_ms: 10_000, stopped_ms: None };
    assert_eq!(starting_tick(c, T0).line, "Starting in 10 s");
    assert_eq!(starting_tick(c, T0 + 2_990).line, "Starting in 8 s", "7.01 s left is still 8 on the clock");
    assert_eq!(starting_tick(c, T0 + 9_800).line, "Starting in 1 s");
    assert_eq!(starting_tick(c, T0 + 10_000).line, "Starting…");
    let t = starting_tick(StartingCountdown { stopped_ms: Some(4_000), ..c }, T0 + 60_000);
    assert_eq!((t.line.as_str(), t.paused), ("Paused", true));
}

#[test]
fn the_ring_starts_full_empties_at_the_deadline_and_keeps_its_scale() {
    let (s, _) = starting();
    let p = s.refresh(vec![row("a", wait(10_000), false)], T0, false);
    let c = countdown(&p.blocks[0]);
    assert_eq!(starting_tick(c, T0).fraction, 1.0);
    assert!((starting_tick(c, T0 + 5_000).fraction - 0.5).abs() < 1e-9);
    assert_eq!(starting_tick(c, T0 + 10_000).fraction, 0.0);
    assert_eq!(starting_tick(c, T0 + 30_000).fraction, 0.0);
    // The next look: the full circle is the wait as first seen, not what is left now.
    let p = s.refresh(vec![row("a", wait(10_000), false)], T0 + 4_000, false);
    let c = countdown(&p.blocks[0]);
    assert_eq!(c.total_ms, 10_000);
    assert!((starting_tick(c, T0 + 4_000).fraction - 0.6).abs() < 1e-9);
}

#[test]
fn pausing_stops_the_number_and_resuming_runs_it_again() {
    let (s, fake) = starting();
    s.refresh(vec![row("a", wait(10_000), false)], T0, false);
    let paused = Some(PendingStart { starts_at_ms: 0, remaining_ms: 6_000, paused: true });
    let p = s.refresh(vec![row("a", paused, false)], T0 + 4_000, false);
    let c = countdown(&p.blocks[0]);
    assert_eq!(starting_tick(c, T0 + 60_000).line, "Paused");
    assert_eq!(p.blocks[0].line, "Paused");
    s.toggle_pause("a".into());
    // Resumed: the engine says it starts 11.5 s after T0.
    let p = s.refresh(vec![row("a", wait(11_500), false)], T0 + 5_000, false);
    assert_eq!(starting_tick(countdown(&p.blocks[0]), T0 + 6_000).line, "Starting in 6 s");
    s.toggle_pause("a".into());
    s.start_now("a".into());
    assert_eq!(*fake.pending.lock().unwrap(), ["resume a", "pause a", "start a"]);
}

#[test]
fn a_torrent_that_started_leaves_and_the_notifier_closes_when_none_is_left() {
    let (s, _) = starting();
    s.refresh(vec![row("a", wait(9_000), false), row("b", wait(9_000), false)], T0, false);
    assert_eq!(ids(&s.refresh(vec![row("b", wait(9_000), false)], T0 + 1_000, false)), ["b"]);
    let p = s.refresh(vec![], T0 + 2_000, false);
    assert!(p.blocks.is_empty());
    assert_eq!(p.show, StartingShow::Close);
}

#[test]
fn a_closed_notifier_stays_closed_until_a_new_torrent_waits() {
    let (s, _) = starting();
    s.refresh(vec![row("a", wait(9_000), false)], T0, false);
    s.dismiss_all();
    assert_eq!(s.refresh(vec![row("a", wait(9_000), false)], T0 + 1_000, false).show, StartingShow::Quiet, "the same torrent does not bring it back");
    let p = s.refresh(vec![row("a", wait(9_000), false), row("b", wait(9_000), false)], T0 + 2_000, false);
    assert!(matches!(p.show, StartingShow::Show { .. }), "a new one does");
    s.dismiss_all();
    s.refresh(vec![], T0 + 20_000, false);
    let p = s.refresh(vec![row("a", wait(9_000), false)], T0 + 21_000, false);
    assert!(matches!(p.show, StartingShow::Show { .. }), "once it left the list, a torrent with the same id is new again");
}

#[test]
fn a_torrent_that_waits_for_a_choice_stays_until_it_is_decided_and_its_checklist_is_loaded() {
    let (s, fake) = starting();
    let p = s.refresh(vec![row("t1", None, true)], T0, false);
    assert_eq!(p.show, StartingShow::Show { stays: true });
    assert_eq!(p.close_help.as_deref(), Some("Close this and leave the torrents waiting. They stay in the menu and the main window"));
    let b = &p.blocks[0];
    assert_eq!(b.state, StartingBlockState::Choice { expanded: false });
    assert_eq!(b.line, "Choose what to download");
    assert_eq!(b.chevron, Some(StartingChevron { open: false, help: "Show the files".into(), label: "Expand".into() }));
    assert_eq!(b.actions.start.as_ref().map(|a| a.help.as_str()), Some("Start downloading every file. Open the list to choose"));
    assert!(b.active, "a lone block owns the keys");
    assert_eq!(fake.loads.load(Ordering::SeqCst), 1, "the checklist is loaded with the first look");
    assert_eq!(b.detail, format!("Movie · 1.0 GB · {}", at_home("Media/Movies")));
    let p = s.refresh(vec![row("t1", None, true)], T0 + 1_000, false);
    assert_eq!(fake.loads.load(Ordering::SeqCst), 1, "and kept");
    // Once loaded, the chooser's own place stands in the line (it follows a category change at once).
    assert_eq!(p.blocks[0].detail, format!("Movie · 1.0 GB · {}", at_home("Media/TV")));
    s.toggle_expanded("t1".into());
    let b = &s.page_now(T0).blocks[0];
    assert_eq!(b.state, StartingBlockState::Choice { expanded: true });
    assert_eq!(b.chevron.as_ref().map(|c| c.help.as_str()), Some("Back to the short view"));
    assert_eq!(b.actions.start.as_ref().map(|a| a.help.as_str()), Some("Start downloading the ticked files"));
}

#[test]
fn start_on_a_choice_sends_the_ticks_and_cancel_happens_once() {
    let (s, fake) = starting();
    s.refresh(vec![row("t1", None, true), row("t2", None, true)], T0, false);
    s.expand("t1".into());
    s.confirm("t1".into()).unwrap();
    assert_eq!(*fake.confirmed.lock().unwrap(), [("t1".to_string(), vec![true, true, false])]);
    assert!(s.page_now(T0).blocks.iter().all(|b| b.state != StartingBlockState::Choice { expanded: true }), "the open block closes");
    assert_eq!(s.cancel("t2".into(), true), StartingCancel { cancelled: true, note: None });
    assert_eq!(s.cancel("t9".into(), false), StartingCancel { cancelled: false, note: None }, "not in the notifier: nothing");
    assert_eq!(*fake.cancelled.lock().unwrap(), [("t2".to_string(), true)]);
}

#[test]
fn choose_files_ends_the_countdown_and_opens_the_checklist_here() {
    let (s, fake) = starting();
    s.refresh(vec![row("t1", wait(8_000), false)], T0, false);
    let b = &s.page_now(T0).blocks[0];
    assert_eq!(b.chevron.as_ref().map(|c| c.help.as_str()), Some("Choose files: ends the countdown and shows the files"));
    assert_eq!(b.actions.start.as_ref().map(|a| a.help.as_str()), Some("Skip the wait and start downloading now"));
    *fake.choose.lock().unwrap() = Some(true);
    assert_eq!(s.choose_files("t1".into()), StartingChoose::Open);
    // Between the click and the engine's answer the row is neither counting nor waiting: still open.
    let p = s.refresh(vec![row("t1", None, false)], T0 + 500, false);
    assert!(p.blocks.is_empty());
    let p = s.refresh(vec![row("t1", None, true)], T0 + 1_000, false);
    assert_eq!(p.blocks[0].state, StartingBlockState::Choice { expanded: true });
    assert!(p.blocks[0].active);
    assert_eq!(p.show, StartingShow::Show { stays: true });
    // It goes with its torrent.
    s.refresh(vec![], T0 + 2_000, false);
    s.refresh(vec![row("t1", None, true)], T0 + 3_000, false);
    assert_eq!(s.page_now(T0).blocks[0].state, StartingBlockState::Choice { expanded: false });
    *fake.choose.lock().unwrap() = None;
    assert_eq!(s.choose_files("t1".into()), StartingChoose::Failed);
}

#[test]
fn a_magnet_link_fetches_its_file_list_then_shows_the_checklist() {
    let (s, fake) = starting();
    assert!(s.refresh(vec![magnet("t1")], T0, false).blocks.is_empty(), "a magnet link nobody asked about stays out");
    s.refresh(vec![row("t1", wait(8_000), false)], T0, false);
    *fake.choose.lock().unwrap() = Some(false);
    assert_eq!(s.choose_files("t1".into()), StartingChoose::AfterMetadata);
    let p = s.refresh(vec![magnet("t1")], T0 + 1_000, false);
    let b = &p.blocks[0];
    assert_eq!(b.state, StartingBlockState::Fetching { line: "Fetching file list…".into() });
    assert_eq!((b.detail.as_str(), b.category.is_none(), b.chevron.is_none()), (format!("Magnet link · {}", at_home("Downloads")).as_str(), true, true));
    assert!(b.actions.start.is_none() && b.actions.keep_file.is_none(), "Cancel only");
    assert_eq!(b.actions.cancel.keys, [HeldKey::Escape]);
    assert_eq!(p.show, StartingShow::Show { stays: true }, "the notifier stays while the list is coming");
    assert!(s.consume_request("t1".into()));
    assert!(!s.consume_request("t1".into()));
    let p = s.refresh(vec![row("t1", None, true)], T0 + 5_000, false);
    assert_eq!(p.blocks[0].state, StartingBlockState::Choice { expanded: true });
}

#[test]
fn torrents_already_waiting_at_launch_do_not_pop_up() {
    let (s, _) = starting();
    assert_eq!(s.refresh(vec![row("t1", None, true)], T0, true).show, StartingShow::Quiet);
    assert!(matches!(s.refresh(vec![row("t1", None, true), row("t2", None, true)], T0, false).show, StartingShow::Show { .. }));
}

#[test]
fn only_one_checklist_is_open_and_it_owns_return_and_escape() {
    let (s, fake) = starting();
    fake.sources.lock().unwrap().push("a".into());
    let p = s.refresh(vec![row("a", None, true), row("b", None, true)], T0, false);
    assert!(p.blocks.iter().all(|b| !b.active && b.actions.cancel.keys.is_empty()), "compact first: nobody owns the keys");
    assert!(p.blocks[0].actions.keep_file.is_some() && p.blocks[1].actions.keep_file.is_none(), "only a .torrent file can be kept");
    s.expand("a".into());
    s.expand("b".into());
    let p = s.page_now(T0);
    assert_eq!(p.blocks.iter().map(|b| b.active).collect::<Vec<_>>(), [false, true], "one block at a time");
    let start = p.blocks[1].actions.start.as_ref().unwrap();
    assert_eq!(start.keys, [HeldKey::Return]);
    s.toggle_expanded("b".into());
    s.collapse("a".into());
    assert!(s.page_now(T0).blocks.iter().all(|b| !b.active));
}

#[test]
fn the_title_opens_the_window_after_ending_a_countdown() {
    let (s, fake) = starting();
    s.refresh(vec![row("a", wait(8_000), false), row("b", None, true)], T0, false);
    *fake.choose.lock().unwrap() = Some(true);
    s.open("a".into());
    s.open("b".into());
    assert_eq!(*fake.pending.lock().unwrap(), ["choose a"], "a choice waits already");
}

#[test]
fn the_category_pill_names_the_category_once_the_files_are_known() {
    let (s, fake) = starting();
    let p = s.refresh(vec![row("t1", None, true)], T0, false);
    let c = p.blocks[0].category.as_ref().unwrap();
    assert_eq!((c.title.as_str(), c.help.as_str()), ("Movie", "Category: Movie — click to change"));
    assert_eq!(c.choices.iter().filter(|x| x.checked).count(), 1);
    s.change_category("t1".into(), Category::Music);
    assert_eq!(*fake.categories.lock().unwrap(), [Category::Music]);
}

#[test]
fn the_folder_is_where_the_files_go_written_from_home() {
    let home = Some("/Users/me");
    if cfg!(unix) {
        assert_eq!(folder_of("/Users/me/Media/Movies/X", home).as_deref(), Some("~/Media/Movies"));
        assert_eq!(folder_of("/Users/me/X", home).as_deref(), Some("~"));
    } else {
        assert_eq!(folder_of(r"C:\Users\me\Media\Movies\X", Some(r"C:\Users\me")).as_deref(), Some(r"C:\Users\me\Media\Movies"));
    }
    assert_eq!(folder_of("/Volumes/Big/Movies/X", home).as_deref(), Some("/Volumes/Big/Movies"));
    assert_eq!(folder_of("", home), None);
    assert_eq!(folder_of("X", home).as_deref(), Some("X"));
}
