use super::*;
use crate::chat_text::tests::{channel, topic};
use crate::library_rows::tests::{download, seed};
use crate::library_rows::{GroupMore, ListCommand, ListGroup, RowControlKind};
use crate::row_text::TextTone;
use crate::sidebar::{tracker_order, SidebarCommand, SidebarIcon};
use crate::sync::SyncRole;
use crate::types::{PendingStart, SharingCounts, TrackerSummary, TransferRow, TransferState};
use crate::CoreOptions;

/// 2027-01-15, Unix ms.
pub(crate) const NOW_MS: u64 = 1_800_000_000_000;
const NOW_S: u64 = NOW_MS / 1000;

/// Writes times as their numbers, so a test sees which form Rust chose.
pub(crate) struct Clock;

impl LocalClock for Clock {
    fn time_of_day(&self, unix_s: u64) -> String {
        format!("T{unix_s}")
    }
    fn weekday_time(&self, unix_s: u64) -> String {
        format!("W{unix_s}")
    }
    fn day(&self, unix_s: u64, with_year: bool) -> String {
        if with_year { format!("D{unix_s}Y") } else { format!("D{unix_s}") }
    }
    fn date_time(&self, unix_s: u64) -> String {
        format!("DT{unix_s}")
    }
    fn utc_offset_s(&self, _: u64) -> i32 {
        3_600
    }
}

pub(crate) fn tracker(id: &str, count: u32, favorite: bool) -> TrackerSummary {
    TrackerSummary { id: id.into(), name: id.into(), count, favorite, hidden: false, hide_only_seeding: false, private: false }
}

pub(crate) fn snap() -> Snapshot {
    Snapshot {
        down_rate: 0,
        up_rate: 0,
        downloading: Vec::new(),
        queued: Vec::new(),
        recent: Vec::new(),
        hidden_downloading: Vec::new(),
        hidden_queued: Vec::new(),
        hidden_recent: Vec::new(),
        sharing_busiest: Vec::new(),
        sharing: SharingCounts::default(),
        sharing_total: 0,
        trackers: vec![tracker("public", 0, false)],
        alt_speed: false,
        down_limit: None,
        up_limit: None,
        error_count: 0,
        moving_percent: None,
        attention: false,
        listen_port: 0,
        connections: 0,
        online: true,
        offline_since_ms: None,
    }
}

pub(crate) fn sync() -> SyncSnapshot {
    SyncSnapshot { running: true, problem: None, device_id: "me".into(), device_name: String::new(), port: 0, folders: Vec::new(), devices: Vec::new() }
}

pub(crate) fn inputs(snapshot: Snapshot) -> Inputs {
    Inputs { snapshot, sync: sync(), channels: Vec::new(), topics: Vec::new(), directs: Vec::new(), sharing: None, tracker_seeds: Vec::new() }
}

pub(crate) fn state(place: LibraryPlace) -> LibraryState {
    let mut st = LibraryState::new(Vec::new());
    st.place = place;
    st
}

pub(crate) fn model(i: &Inputs, st: &LibraryState) -> LibraryViewModel {
    build(i, st, NOW_MS, &Clock)
}

pub(crate) fn on(r: TransferRow, tracker: &str) -> TransferRow {
    TransferRow { tracker_id: tracker.into(), tracker_name: tracker.into(), ..r }
}

pub(crate) fn named(id: &str, r: TransferRow) -> TransferRow {
    TransferRow { id: id.into(), name: id.into(), display_name: id.into(), ..r }
}

fn keys(groups: &[ListGroup]) -> Vec<&str> {
    groups.iter().map(|g| g.key.as_str()).collect()
}

pub(crate) fn ids(g: &ListGroup) -> Vec<&str> {
    g.rows.iter().map(|r| r.id.as_str()).collect()
}

pub(crate) fn list(m: &LibraryViewModel) -> &ListModel {
    m.list.as_ref().expect("a list page")
}

pub(crate) fn group<'a>(m: &'a LibraryViewModel, key: &str) -> &'a ListGroup {
    list(m).groups.iter().find(|g| g.key == key).unwrap_or_else(|| panic!("no group {key}"))
}

pub(crate) fn sharing(groups: &[(SeedGroup, Vec<TransferRow>)]) -> SharingRows {
    let mut counts = SharingCounts::default();
    for (g, rows) in groups {
        for _ in rows {
            counts.add(*g);
        }
    }
    SharingRows { counts, rows: groups.to_vec() }
}

pub(crate) fn seeding(id: &str, g: SeedGroup) -> TransferRow {
    named(id, TransferRow { seed_group: Some(g), ..seed() })
}

pub(crate) fn mock() -> (Arc<Core>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let core = Core::new(CoreOptions { config_dir: dir.path().display().to_string(), mock: true, allow_loopback_trackers: false, forbidden_root: None }).unwrap();
    (core, dir)
}

pub(crate) fn library(core: &Arc<Core>) -> Arc<Library> {
    Library::new(core.clone(), Vec::new(), Arc::new(Clock))
}

// MARK: Downloads

#[test]
fn downloads_lists_the_active_ones_then_up_next_then_recently_finished() {
    let mut s = snap();
    s.downloading = vec![named("a", download(TransferState::Downloading))];
    s.queued = vec![named("q", download(TransferState::Queued))];
    s.recent = vec![seeding("r", SeedGroup::Active)];
    let m = model(&inputs(s), &state(LibraryPlace::Downloads));
    assert_eq!((m.page, m.title.as_str()), (LibraryPage::List, "Downloads"));
    let l = list(&m);
    assert_eq!(keys(&l.groups), ["active", "next", "recent"]);
    assert_eq!(l.groups.iter().map(|g| g.title.as_deref()).collect::<Vec<_>>(), [None, Some("Up Next"), Some("Recently Finished")]);
    assert!(l.groups.iter().all(|g| g.count.is_none() && !g.block && g.parent.is_none()));
    assert_eq!(group(&m, "next").fold_help.as_deref(), Some("Hide Up Next"));
    assert_eq!(group(&m, "active").fold_help, None);
    let clear = group(&m, "recent").action.clone().unwrap();
    assert_eq!((clear.title.as_str(), clear.help.as_str(), clear.command), ("Clear", "Clear for this whole group", ListCommand::ClearRecent));
    // Each row in its group's style.
    assert_eq!(group(&m, "next").rows[0].controls.iter().map(|c| c.kind).collect::<Vec<_>>(), [RowControlKind::GetInfo]);
    assert_eq!(group(&m, "recent").rows[0].controls.last().unwrap().kind, RowControlKind::Reveal);
    assert_eq!((l.empty.as_deref(), l.filter_chip.as_ref()), (None, None));
}

#[test]
fn the_downloads_summary_counts_what_waits_and_both_rates() {
    let mut s = snap();
    s.down_rate = 2_100_000;
    s.up_rate = 340_000;
    let choice = TransferRow { awaiting_choice: true, ..named("c", download(TransferState::Queued)) };
    let pending = TransferRow { pending: Some(PendingStart { starts_at_ms: 0, remaining_ms: 7_000, paused: false }), ..named("p", download(TransferState::Queued)) };
    s.downloading = vec![choice, pending, named("a", download(TransferState::Downloading))];
    s.queued = vec![named("q", download(TransferState::Queued)), named("q2", download(TransferState::Queued))];
    s.recent = vec![TransferRow { finished_at: Some(NOW_S - 60), ..seeding("r", SeedGroup::Active) }];
    let m = model(&inputs(s.clone()), &state(LibraryPlace::Downloads));
    assert_eq!(m.summary, "1 active · 2 up next · 1 to choose · 1 starting · 1 finished today · ↓ 2.1 MB/s · ↑ 340 KB/s");
    s.downloading.clear();
    s.recent.clear();
    s.queued.clear();
    assert_eq!(model(&inputs(s), &state(LibraryPlace::Downloads)).summary, "0 active · 0 up next · ↓ 2.1 MB/s · ↑ 340 KB/s");
}

#[test]
fn finished_today_counts_only_what_finished_on_the_local_calendar_day() {
    // The clock is an hour east of UTC: NOW_S is 09:00 there, so the local day began 32 400 s ago, which is still the
    // UTC day before for its first hour.
    let midnight = NOW_S - 32_400;
    let finished = |id: &str, at: Option<u64>| TransferRow { finished_at: at, ..seeding(id, SeedGroup::Active) };
    let mut s = snap();
    s.recent = vec![
        finished("now", Some(NOW_S - 60)),
        finished("midnight", Some(midnight)),
        finished("early", Some(midnight + 1_800)),
        finished("yesterday", Some(midnight - 1)),
        finished("days ago", Some(NOW_S - 3 * DAY_S as u64)),
        finished("unknown", None),
    ];
    let m = model(&inputs(s.clone()), &state(LibraryPlace::Downloads));
    assert_eq!(m.summary, "0 active · 0 up next · 3 finished today · ↓ 0 KB/s · ↑ 0 KB/s");
    assert_eq!(group(&m, "recent").rows.len(), 6, "the list itself keeps them all");
    // Nothing finished today: the part is left out, the list still shows the older ones.
    s.recent.drain(..3);
    let m = model(&inputs(s), &state(LibraryPlace::Downloads));
    assert_eq!(m.summary, "0 active · 0 up next · ↓ 0 KB/s · ↑ 0 KB/s");
    assert_eq!(group(&m, "recent").rows.len(), 3);
}

#[test]
fn the_search_and_the_category_narrow_the_downloads() {
    let mut s = snap();
    let tv = TransferRow { category: Category::Tv, ..named("Fallen.Kingdom.S02", download(TransferState::Downloading)) };
    s.downloading = vec![named("Sintel", download(TransferState::Downloading)), tv];
    let i = inputs(s);
    let mut st = state(LibraryPlace::Downloads);
    st.search = "FALLEN".into();
    assert_eq!(ids(&list(&model(&i, &st)).groups[0]), ["Fallen.Kingdom.S02"], "whatever the case");
    st.search.clear();
    st.category = Some(Category::Tv);
    let m = model(&i, &st);
    assert_eq!(ids(&list(&m).groups[0]), ["Fallen.Kingdom.S02"]);
    assert_eq!(list(&m).filter_chip.as_ref().map(|c| c.category), Some(Category::Tv));
    assert!(list(&m).groups[0].rows[0].pill.active);
    st.category = Some(Category::Music);
    let m = model(&i, &st);
    assert_eq!((list(&m).groups.len(), list(&m).empty.as_deref()), (0, Some("No matches")));
    let none = model(&inputs(snap()), &state(LibraryPlace::Downloads));
    assert_eq!(list(&none).empty.as_deref(), Some("No downloads"));
}

#[test]
fn a_folded_group_keeps_its_header_and_ships_no_rows() {
    let mut s = snap();
    s.downloading = vec![named("a", download(TransferState::Downloading))];
    s.queued = vec![named("q", download(TransferState::Queued))];
    let mut st = state(LibraryPlace::Downloads);
    st.folded = ["downloads.next".to_owned(), "downloads.active".to_owned()].into();
    let m = model(&inputs(s), &st);
    let next = group(&m, "next");
    assert!(next.folded && next.rows.is_empty());
    assert_eq!(next.fold_help.as_deref(), Some("Show Up Next"));
    assert!(!group(&m, "active").folded && ids(group(&m, "active")) == ["a"], "a group without a header never folds");
}

#[test]
fn hidden_trackers_stay_out_of_downloads_but_not_out_of_their_own_tab() {
    let mut s = snap();
    s.downloading = vec![named("pub", download(TransferState::Downloading))];
    s.hidden_downloading = vec![on(named("h", download(TransferState::Downloading)), "h")];
    s.hidden_queued = vec![on(named("hq", download(TransferState::Queued)), "h")];
    s.trackers.push(TrackerSummary { hidden: true, ..tracker("h", 2, true) });
    let i = inputs(s);
    assert_eq!(ids(&list(&model(&i, &state(LibraryPlace::Downloads))).groups[0]), ["pub"]);
    let m = model(&Inputs { sharing: Some(SharingRows::default()), ..i }, &state(LibraryPlace::Tracker { tracker_id: "h".into() }));
    assert_eq!(keys(&list(&m).groups), ["downloads", "downloading", "queued"]);
    assert_eq!(ids(group(&m, "downloading")), ["h"]);
}

// MARK: Sharing

#[test]
fn sharing_lists_its_groups_with_counts_hints_and_more() {
    let dormant: Vec<TransferRow> = (0..150).map(|n| seeding(&format!("d{n}"), SeedGroup::Dormant)).collect();
    let mut rows = sharing(&[(SeedGroup::Popular, vec![seeding("p", SeedGroup::Popular)]), (SeedGroup::Dormant, dormant), (SeedGroup::Dead, vec![seeding("x", SeedGroup::Dead)])]);
    rows.counts.dormant = 200;
    rows.counts.error = 0;
    let mut s = snap();
    s.sharing_total = 212;
    s.up_rate = 1_000;
    let i = Inputs { sharing: Some(rows), ..inputs(s) };
    let m = model(&i, &state(LibraryPlace::Sharing));
    assert_eq!((m.title.as_str(), m.summary.as_str()), ("Sharing", "212 torrents · ↓ 0 KB/s · ↑ 1.0 KB/s"));
    assert_eq!(keys(&list(&m).groups), ["popular", "dormant", "dead"]);
    let d = group(&m, "dormant");
    assert_eq!((d.title.as_deref(), d.count.as_deref(), d.rows.len()), (Some("Dormant"), Some("200"), 150));
    assert_eq!(d.hint.as_deref(), Some("Resting in low power: still announcing on schedule, wakes instantly when a peer asks."));
    assert_eq!(d.more, Some(GroupMore { title: "Show 50 more…".into(), help: "Show the other 50 in this list".into(), group: SeedGroup::Dormant }));
    let dead = group(&m, "dead");
    assert_eq!((dead.tone, dead.hint.as_deref()), (TextTone::Problem, Some("Nothing is removed automatically.")));
    assert_eq!((group(&m, "popular").tone, group(&m, "popular").hint.as_deref(), group(&m, "popular").more.as_ref()), (TextTone::Secondary, None, None));
    assert!(d.rows.iter().all(|r| r.dimmed));
    // Folded, the count stays and "Show more" goes.
    let mut st = state(LibraryPlace::Sharing);
    st.folded.insert("sharing.dormant".into());
    let folded = model(&Inputs { sharing: Some(SharingRows { rows: vec![], ..i.sharing.clone().unwrap() }), ..i.clone() }, &st);
    let d = group(&folded, "dormant");
    assert!(d.folded && d.rows.is_empty() && d.more.is_none() && d.count.as_deref() == Some("200"));
    let empty = model(&Inputs { sharing: Some(SharingRows::default()), ..inputs(snap()) }, &state(LibraryPlace::Sharing));
    assert_eq!(list(&empty).empty.as_deref(), Some("Nothing is being shared"));
}

#[test]
fn an_error_group_says_it_retries() {
    let i = Inputs { sharing: Some(sharing(&[(SeedGroup::Error, vec![seeding("e", SeedGroup::Error)])])), ..inputs(snap()) };
    let m = model(&i, &state(LibraryPlace::Sharing));
    assert_eq!(group(&m, "error").hint.as_deref(), Some("Retrying automatically."));
    assert_eq!(group(&m, "error").rows[0].controls[0].kind, RowControlKind::ErrorBadge);
}

// MARK: A tracker's tab

pub(crate) fn tab(s: Snapshot, rows: SharingRows) -> LibraryViewModel {
    let mut s = s;
    s.trackers.push(tracker("t1", 9, false));
    model(&Inputs { sharing: Some(rows), ..inputs(s) }, &state(LibraryPlace::Tracker { tracker_id: "t1".into() }))
}

pub(crate) fn t1(id: &str, r: TransferRow) -> TransferRow {
    on(named(id, r), "t1")
}

#[test]
fn a_tracker_tab_has_blocks_and_subgroups_in_order_with_counts() {
    let mut s = snap();
    s.downloading = vec![t1("a", download(TransferState::Downloading)), on(named("other", download(TransferState::Downloading)), "t2")];
    s.queued = vec![t1("q", download(TransferState::Queued))];
    let m = tab(s, sharing(&[(SeedGroup::Popular, vec![t1("p", seed())]), (SeedGroup::Dormant, vec![t1("d", seed())])]));
    let l = list(&m);
    assert_eq!(keys(&l.groups), ["downloads", "downloading", "queued", "sharing", "popular", "dormant"]);
    let head = &l.groups[0];
    assert!(head.block && head.rows.is_empty() && head.count.as_deref() == Some("2") && head.title.as_deref() == Some("Downloads"));
    assert_eq!(group(&m, "sharing").count.as_deref(), Some("2"));
    assert_eq!(l.groups.iter().filter(|g| g.parent.as_deref() == Some("downloads")).map(|g| g.key.as_str()).collect::<Vec<_>>(), ["downloading", "queued"]);
    assert_eq!(group(&m, "downloading").count.as_deref(), Some("1"));
    assert_eq!(ids(group(&m, "downloading")), ["a"], "only this tracker's");
    assert_eq!((m.title.as_str(), m.summary.as_str(), m.title_star), ("t1", "Tracker", false));
}

#[test]
fn empty_groups_and_blocks_are_left_out() {
    let m = tab(snap(), SharingRows::default());
    assert!(list(&m).groups.is_empty());
    assert_eq!(list(&m).empty.as_deref(), Some("No torrents on t1"));
    assert_eq!(keys(&list(&tab(snap(), sharing(&[(SeedGroup::Active, vec![t1("x", seed())])]))).groups), ["sharing", "active"]);
    let mut s = snap();
    s.queued = vec![t1("q", download(TransferState::Queued))];
    assert_eq!(keys(&list(&tab(s, SharingRows::default())).groups), ["downloads", "queued"]);
}

#[test]
fn a_finished_torrent_moves_to_sharing_and_a_direct_download_stays() {
    // Just finished: still in the recent list, already a seed. It is listed once, as a seed.
    let mut s = snap();
    s.recent = vec![t1("a", seed())];
    let m = tab(s, sharing(&[(SeedGroup::Active, vec![t1("a", seed())])]));
    assert_eq!(keys(&list(&m).groups), ["sharing", "active"]);
    assert_eq!(ids(group(&m, "active")), ["a"]);
    let mut s = snap();
    let web = TransferRow { kind: TransferKind::Direct, state: TransferState::Completed, ..t1("w", seed()) };
    s.recent = vec![web, t1("t", seed())];
    let m = tab(s, SharingRows::default());
    assert_eq!(keys(&list(&m).groups), ["downloads", "recent"]);
    assert_eq!(ids(group(&m, "recent")), ["w"]);
}

#[test]
fn a_folded_block_hides_its_groups_and_keeps_its_count() {
    let mut s = snap();
    s.downloading = vec![t1("a", download(TransferState::Downloading))];
    s.queued = vec![t1("q", download(TransferState::Queued))];
    s.trackers.push(tracker("t1", 9, true));
    let mut st = state(LibraryPlace::Tracker { tracker_id: "t1".into() });
    st.folded.insert("tracker.downloads".into());
    let m = model(&Inputs { sharing: Some(sharing(&[(SeedGroup::Active, vec![t1("x", seed())])])), ..inputs(s) }, &st);
    assert_eq!(keys(&list(&m).groups), ["downloads", "sharing", "active"]);
    let block = group(&m, "downloads");
    assert!(block.folded && block.count.as_deref() == Some("2"));
    assert_eq!(block.fold_help.as_deref(), Some("Show Downloads"));
    assert_eq!((m.title_star, m.summary.as_str()), (true, "Favorite tracker"));
}

#[test]
fn a_tracker_tab_leaves_its_name_out_of_the_rows() {
    let mut dead = t1("x", seed());
    dead.seed_group = Some(SeedGroup::Dead);
    dead.error = Some("Unregistered torrent".into());
    let m = tab(snap(), sharing(&[(SeedGroup::Dead, vec![dead.clone()])]));
    assert_eq!(group(&m, "dead").rows[0].status[0].text, "Unregistered torrent");
    let m = model(&Inputs { sharing: Some(sharing(&[(SeedGroup::Dead, vec![dead])])), ..inputs(snap()) }, &state(LibraryPlace::Sharing));
    assert_eq!(group(&m, "dead").rows[0].status[0].text, "t1 · ");
}

#[test]
fn the_tracker_rates_count_all_its_torrents_once_and_only_its_own() {
    let mut s = snap();
    let a = TransferRow { down_rate: 100, up_rate: 5, ..t1("a", download(TransferState::Downloading)) };
    s.downloading = vec![a.clone(), TransferRow { up_rate: 99, ..on(named("c", download(TransferState::Downloading)), "t2") }];
    s.hidden_queued = vec![TransferRow { down_rate: 1, ..t1("h", download(TransferState::Queued)) }];
    s.trackers.push(tracker("t1", 9, false));
    // The list shows one seed of three (a page, a search, a fold): the rates count all three.
    let b = TransferRow { up_rate: 7, ..t1("b", seed()) };
    let seeds = vec![b.clone(), TransferRow { up_rate: 1_000, ..t1("beyond", seed()) }, TransferRow { up_rate: 3, ..t1("folded", seed()) }];
    let i = Inputs { sharing: Some(sharing(&[(SeedGroup::Active, vec![b])])), tracker_seeds: seeds, ..inputs(s) };
    let mut st = state(LibraryPlace::Tracker { tracker_id: "t1".into() });
    assert_eq!(model(&i, &st).tracker_rates, Some(TrackerRates { down: 101, up: 1_015 }));
    st.tracker_info = true;
    assert_eq!(model(&i, &st).tracker_rates, Some(TrackerRates { down: 101, up: 1_015 }), "its Info page too");
    assert_eq!(model(&inputs(snap()), &state(LibraryPlace::Downloads)).tracker_rates, None);
}

#[test]
fn the_tracker_rates_of_the_mock_count_seeds_past_the_first_page() {
    let (core, _d) = mock();
    let lib = library(&core);
    let id = "tracker-z.cc";
    lib.select_place(LibraryPlace::Tracker { tracker_id: id.into() });
    lib.set_search("no such torrent".into());
    let rates = lib.model(NOW_MS).tracker_rates.unwrap();
    let s = lib.snapshot();
    let q = RowQuery { group: None, tracker_id: Some(id.into()), text: String::new(), category: None, offset: 0, limit: u32::MAX };
    let seeds = core.sharing_rows(q);
    assert!(seeds.len() > 150 || seeds.iter().any(|r| r.up_rate > 0), "the mock's tracker has seeds that move");
    let downloads = s.downloading.iter().chain(&s.hidden_downloading).chain(&s.queued).chain(&s.hidden_queued).filter(|r| r.tracker_id == id);
    let up: u64 = downloads.clone().map(|r| r.up_rate).sum::<u64>() + seeds.iter().map(|r| r.up_rate).sum::<u64>();
    assert_eq!(rates.up, up, "nothing listed (the search matches none), every seed counted");
    assert_eq!(rates.down, downloads.map(|r| r.down_rate).sum::<u64>() + seeds.iter().map(|r| r.down_rate).sum::<u64>());
}

#[test]
fn a_tracker_tab_with_a_filter_says_no_matches_on_it() {
    let mut s = snap();
    s.trackers.push(tracker("t1", 9, false));
    let mut st = state(LibraryPlace::Tracker { tracker_id: "t1".into() });
    st.search = "x".into();
    assert_eq!(list(&model(&Inputs { sharing: Some(SharingRows::default()), ..inputs(s) }, &st)).empty.as_deref(), Some("No matches on t1"));
}

#[test]
fn a_trackers_info_page_has_no_list() {
    let mut s = snap();
    s.trackers.push(tracker("t1", 9, true));
    let mut st = state(LibraryPlace::Tracker { tracker_id: "t1".into() });
    st.tracker_info = true;
    let m = model(&inputs(s.clone()), &st);
    assert_eq!((m.page, m.summary.as_str(), m.list.is_none()), (LibraryPage::TrackerInfo, "Info", true));
    // A tracker that is gone (the fallback moves on at the next read).
    let gone = model(&inputs(snap()), &st);
    assert_eq!((gone.page, gone.title.as_str(), gone.summary.as_str()), (LibraryPage::List, "Tracker", "t1"));
}

// MARK: The other places

#[test]
fn chats_name_the_channel_the_topic_and_the_person() {
    let mut team = channel("Team", 3, 2, 5, false);
    team.share_until_s = Some(NOW_S + 10 * 86_400);
    let mut i = inputs(snap());
    i.channels = vec![team.clone()];
    i.topics = vec![vec![topic("Dinner", "Team", 1)]];
    let general = model(&i, &state(LibraryPlace::Chat { channel_id: "Team".into(), topic_id: None }));
    assert_eq!((general.page, general.title.as_str()), (LibraryPage::Conversation, "Team"));
    assert_eq!(general.summary, format!("Owner · 2 of 3 online · shared until D{}", NOW_S + 10 * 86_400), "this year: no year");
    i.channels[0].share_until_s = Some(NOW_S + 400 * 86_400);
    let later = model(&i, &state(LibraryPlace::Chat { channel_id: "Team".into(), topic_id: None }));
    assert!(later.summary.ends_with(&format!("shared until D{}Y", NOW_S + 400 * 86_400)));
    let t = model(&i, &state(LibraryPlace::Chat { channel_id: "Team".into(), topic_id: Some("Dinner".into()) }));
    assert_eq!((t.title.as_str(), t.summary.as_str()), ("Team › Dinner", "Topic by Kari's iPhone · 12 messages"));
    let gone = model(&i, &state(LibraryPlace::Chat { channel_id: "Team".into(), topic_id: Some("Old".into()) }));
    assert_eq!((gone.page, gone.missing.as_deref(), gone.title.as_str()), (LibraryPage::Missing, Some("This topic is no longer in the channel"), "Team"));
    i.directs = vec![SyncDirect { device_id: "d1".into(), name: "Studio iMac".into(), user: "Kari".into(), online: true, unread: 2, last_message_s: None }];
    let d = model(&i, &state(LibraryPlace::Direct { device_id: "d1".into() }));
    assert_eq!((d.page, d.title.as_str(), d.summary.as_str()), (LibraryPage::Conversation, "Kari (Studio iMac)", "Private · connected · end-to-end encrypted"));
    let mut st = state(LibraryPlace::Direct { device_id: "new".into() });
    assert_eq!(model(&i, &st).title, "Unnamed device");
    st.direct_names.insert("new".into(), "Kari's iPhone".into());
    let m = model(&i, &st);
    assert_eq!((m.title.as_str(), m.summary.as_str()), ("Kari's iPhone", "Private · not connected · end-to-end encrypted"));
    // The conversation just started is listed first in the sidebar.
    let directs = m.sidebar.directs.unwrap();
    assert_eq!(directs.title, "Direct Messages");
    assert_eq!(directs.items.iter().map(|x| x.title.as_str()).collect::<Vec<_>>(), ["Kari's iPhone", "Kari (Studio iMac)"]);
    assert_eq!((directs.items[1].online, directs.items[1].unread, directs.items[1].help.as_str()), (Some(true), 2, "Private conversation with Kari (Studio iMac)"));
    assert_eq!(directs.items[1].icon, SidebarIcon::Person);
}

#[cfg(target_os = "macos")]
#[test]
fn the_mac_says_mac_finder_and_its_shortcuts() {
    let mut i = inputs(snap());
    i.sync.device_name = "Studio iMac".into();
    let fs = model(&i, &state(LibraryPlace::FolderSync));
    assert_eq!((fs.page, fs.title.as_str(), fs.summary.as_str()), (LibraryPage::FolderSync, "Folder Sync", "No folders · This Mac: Studio iMac"));
    let gone = model(&i, &state(LibraryPlace::Chat { channel_id: "x".into(), topic_id: None }));
    assert_eq!((gone.title.as_str(), gone.summary.as_str()), ("Channel", "No longer on this Mac"));
    assert_eq!((gone.page, gone.missing.as_deref()), (LibraryPage::Missing, Some("This channel is no longer on this Mac")));
    let mut s = snap();
    s.alt_speed = true;
    let f = model(&inputs(s), &state(LibraryPlace::Settings)).sidebar.footer;
    assert_eq!((f.settings_help.as_str(), f.settings_label.as_str(), f.settings_active), ("Settings (⌘,)", "Settings", true));
    assert_eq!(f.limit_speed.as_deref(), Some("Limit Speed is on (⌥⌘L)"));
    let mut done = inputs(snap());
    done.snapshot.recent = vec![seed()];
    let m = model(&done, &state(LibraryPlace::Downloads));
    assert_eq!(group(&m, "recent").rows[0].controls.last().unwrap().help, "Show in Finder");
    assert_eq!(crate::chat_text::subtitle(&channel("Solo", 1, 0, 0, false)), "Only this Mac");
}

#[test]
fn settings_and_folder_sync_have_their_own_pages() {
    let m = model(&inputs(snap()), &state(LibraryPlace::Settings));
    assert_eq!((m.page, m.title.as_str(), m.summary.as_str(), m.list.is_none()), (LibraryPage::Settings, "Settings", "Settings", true));
    let fs = model(&inputs(snap()), &state(LibraryPlace::FolderSync));
    assert_eq!(fs.summary, "No folders", "no device name: nothing after it");
}

// MARK: The sidebar

#[test]
fn the_sidebar_places_count_and_explain_themselves() {
    let mut s = snap();
    s.downloading = vec![named("a", download(TransferState::Downloading))];
    s.queued = vec![named("q", download(TransferState::Queued))];
    s.sharing_total = 212;
    let mut st = state(LibraryPlace::Sharing);
    st.downloads_flag = true;
    let m = model(&inputs(s), &st);
    let p = &m.sidebar.places;
    assert_eq!(p.iter().map(|x| (x.title.as_str(), x.badge)).collect::<Vec<_>>(), [("Downloads", 2), ("Sharing", 212), ("Folder Sync", 0)]);
    assert_eq!(p.iter().map(|x| x.help.as_str()).collect::<Vec<_>>(), ["Torrents that are downloading or waiting", "Torrents you are seeding", "Folders kept the same on your devices"]);
    assert_eq!(p[0].flag.as_ref().map(|f| (f.help.as_str(), f.accessibility.as_str())), Some(("A new download started", "New download")));
    assert_eq!(p.iter().map(|x| x.place.clone()).collect::<Vec<_>>(), [LibraryPlace::Downloads, LibraryPlace::Sharing, LibraryPlace::FolderSync]);
}

#[test]
fn the_sidebar_lists_channels_with_their_unread_messages() {
    let mut i = inputs(snap());
    let mut readonly = channel("News", 4, 1, 3, false);
    readonly.role = SyncRole::ReadOnly;
    i.channels = vec![channel("Team", 3, 2, 5, false), channel("Quiet", 2, 1, 4, true), readonly];
    i.topics = vec![vec![topic("Dinner", "Team", 1), topic("Trip", "Team", 2)], vec![topic("Mute", "Quiet", 4)], vec![]];
    let mut st = state(LibraryPlace::Downloads);
    let closed = model(&i, &st).sidebar.chats;
    assert_eq!(closed.title, "Chats");
    assert_eq!(closed.channels[0].item.unread, 5, "closed: the channel's total");
    st.open_channels.insert("Team".into());
    st.open_channels.insert("Quiet".into());
    let chats = model(&i, &st).sidebar.chats;
    let team = &chats.channels[0];
    assert!(team.open);
    assert_eq!(team.item.unread, 2, "open: General's own");
    assert_eq!((team.item.icon, team.item.help.as_str()), (SidebarIcon::Channel, "Team: 2 of 3 online"));
    assert_eq!(team.topics.iter().map(|t| (t.title.as_str(), t.unread)).collect::<Vec<_>>(), [("Dinner", 1), ("Trip", 2)]);
    assert_eq!(team.topics[0].help, "Dinner: Topic by Kari's iPhone · 12 messages");
    assert_eq!(team.topics[0].place, LibraryPlace::Chat { channel_id: "Team".into(), topic_id: Some("Dinner".into()) });
    assert_eq!(team.new_topic.as_ref().map(|a| a.title.as_str()), Some("New Topic…"));
    assert_eq!(team.new_topic.as_ref().unwrap().help, "Start a topic in Team: a conversation of its own, apart from General");
    let quiet = &chats.channels[1];
    assert_eq!((quiet.item.unread, quiet.topics[0].unread, quiet.item.icon), (0, 0, SidebarIcon::MutedChannel), "muted: no counts");
    assert_eq!(quiet.item.help, "Quiet: 1 of 2 online · Muted");
    assert!(chats.channels[2].new_topic.is_none(), "read only: no new topics");
    assert_eq!(chats.add.title, "New Channel…");
    assert_eq!(chats.add.help, "Make a channel with your devices, the people you share with and their agents, or join one with a link");
    assert!(model(&i, &st).sidebar.directs.is_none(), "no Direct Messages without any");
}

#[test]
fn the_sidebar_lists_the_trackers_with_torrents_in_their_order() {
    let mut s = snap();
    s.trackers = vec![
        tracker("public", 40, false),
        tracker("other-private", 2, false),
        tracker("small.org", 3, false),
        tracker("big.org", 30, false),
        tracker("Zeta", 1, true),
        tracker("alpha", 0, true),
        tracker("empty.net", 0, false),
        TrackerSummary { hidden: true, ..tracker("Hidden", 5, true) },
    ];
    let m = model(&inputs(s.clone()), &state(LibraryPlace::Downloads));
    let t = m.sidebar.trackers.unwrap();
    assert_eq!(t.title, "Trackers");
    assert_eq!(t.items.iter().map(|x| x.title.as_str()).collect::<Vec<_>>(), ["Zeta", "big.org", "small.org", "other-private", "public"]);
    assert_eq!(t.items.iter().map(|x| x.icon).collect::<Vec<_>>()[..2], [SidebarIcon::FavoriteTracker, SidebarIcon::Tracker]);
    assert_eq!(t.items.last().map(|x| x.icon), Some(SidebarIcon::PublicTracker));
    assert_eq!((t.items[0].count, t.items[0].help.as_str()), (Some(1), "Favorite tracker: every torrent on Zeta"));
    assert_eq!(t.items[1].help, "Every torrent on big.org, downloading and sharing");
    // The tracker shown stays, even hidden or empty.
    let m = model(&Inputs { sharing: Some(SharingRows::default()), ..inputs(s.clone()) }, &state(LibraryPlace::Tracker { tracker_id: "Hidden".into() }));
    assert_eq!(m.sidebar.trackers.unwrap().items.iter().map(|x| x.title.as_str()).collect::<Vec<_>>(), ["Hidden", "Zeta", "big.org", "small.org", "other-private", "public"]);
    assert_eq!(tracker_order(s.trackers.clone(), Some("alpha".into())).iter().map(|x| x.id.as_str()).collect::<Vec<_>>()[..2], ["alpha", "Zeta"]);
    let mut none = snap();
    none.trackers = vec![tracker("public", 0, false)];
    assert!(model(&inputs(none), &state(LibraryPlace::Downloads)).sidebar.trackers.is_none());
}

#[test]
fn trackers_with_the_same_count_sort_as_the_mac_compares_names() {
    // The order macOS gives these names (nb_NO, `localizedCaseInsensitiveCompare`): spaces and punctuation, then
    // digits, then letters whatever their case and accent; Æ and Ø after Z.
    let mac = [
        "a b", "a_b", "a-b", "a.b", "a@b", "a+b", "a~b", "a1", "ab", "alpha", "Beta", "eagle", "émile", "fox", "tracker_z.cc",
        "tracker-z.cc", "tracker.z.cc", "tracker1.org", "tracker10.org", "Tracker2.org", "trackerz.cc", "Zeta", "Ærlig", "Øya",
    ];
    let mut shuffled: Vec<TrackerSummary> = mac.iter().rev().map(|n| tracker(n, 1, false)).collect();
    shuffled.rotate_left(7);
    let sorted: Vec<String> = crate::sidebar::sorted_trackers(&shuffled).iter().map(|t| t.name.clone()).collect();
    assert_eq!(sorted, mac);
    // Names that differ only in case keep a stable order: by id.
    let same = [TrackerSummary { id: "b".into(), ..tracker("ALPHA", 1, false) }, TrackerSummary { id: "a".into(), ..tracker("alpha", 1, false) }];
    assert_eq!(crate::sidebar::sorted_trackers(&same).iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), ["a", "b"]);
}

#[test]
fn favorites_come_first_then_the_busiest_then_other_private_then_public() {
    let order = |list: &[TrackerSummary]| crate::sidebar::sorted_trackers(list).iter().map(|t| t.id.clone()).collect::<Vec<_>>();
    let t = |id: &str, name: &str, count, favorite| TrackerSummary { name: name.into(), ..tracker(id, count, favorite) };
    let list = [
        t("public", "Public", 40, false),
        t("debian.org", "Debian", 3, false),
        t("zeta.org", "Zeta", 30, true),
        t("other-private", "Other private", 1, false),
        t("127.0.0.1", "127.0.0.1", 9, false),
        t("alpha.org", "alpha", 1, true),
        t("b.test", "Bee", 3, false),
    ];
    assert_eq!(order(&list), ["alpha.org", "zeta.org", "127.0.0.1", "b.test", "debian.org", "other-private", "public"]);
    let same = [t("b.org", "Bravo", 2, false), t("a.org", "alpha", 2, false), t("c.org", "Charlie", 2, false)];
    assert_eq!(order(&same), ["a.org", "b.org", "c.org"], "equal counts fall back to the name");
    assert_eq!(order(&[t("public", "Public", 99, false), t("a.org", "A", 0, false)]).last().map(String::as_str), Some("public"));
    // Hidden or empty trackers are not in the sidebar unless their tab is open.
    let hidden = [t("public", "Public", 4, false), TrackerSummary { hidden: true, ..t("h.org", "Hush", 3, true) }, t("a.org", "A", 2, true)];
    let ids = |v: Vec<TrackerSummary>| v.into_iter().map(|t| t.id).collect::<Vec<_>>();
    assert_eq!(ids(tracker_order(hidden.to_vec(), None)), ["a.org", "public"]);
    assert_eq!(ids(tracker_order(hidden.to_vec(), Some("h.org".into()))), ["a.org", "h.org", "public"]);
    let empty = [t("e.org", "Empty", 0, true), t("public", "Public", 1, false)];
    assert_eq!(ids(tracker_order(empty.to_vec(), None)), ["public"]);
    assert_eq!(ids(tracker_order(empty.to_vec(), Some("e.org".into()))), ["e.org", "public"]);
    let none = [t("public", "Public", 0, false), t("debian.org", "Debian", 2, false), t("fav", "Fav", 0, true)];
    assert_eq!(ids(tracker_order(none.to_vec(), None)), ["debian.org"]);
    assert_eq!(ids(tracker_order(none.to_vec(), Some("public".into()))), ["debian.org", "public"]);
}

#[test]
fn the_footer_shows_the_rates_with_their_limits_or_offline() {
    let mut s = snap();
    (s.down_rate, s.up_rate, s.down_limit) = (2_100_000, 340_000, Some(18_000_000));
    let f = model(&inputs(s.clone()), &state(LibraryPlace::Downloads)).sidebar.footer;
    assert_eq!(f.down.as_ref().map(|r| (r.text.as_str(), r.help.as_str())), Some(("↓ 2.1 MB/s", "Download limit: 18 MB/s")));
    assert_eq!(f.up.as_ref().map(|r| (r.text.as_str(), r.help.as_str())), Some(("↑ 340 KB/s", "No upload limit")));
    assert!(f.limit_speed.is_none() && f.offline.is_none() && !f.settings_active);
    s.online = false;
    let f = model(&inputs(s), &state(LibraryPlace::Downloads)).sidebar.footer;
    assert!(f.down.is_none() && f.up.is_none());
    assert_eq!(
        f.offline.map(|o| (o.text, o.help)),
        Some(("Offline".into(), "No network. Seedwell sends almost nothing until it is back, and picks up by itself.".into()))
    );
}

#[test]
fn the_sidebar_menus_offer_what_each_item_can_do() {
    let titles = |v: Vec<SidebarMenuEntry>| {
        v.into_iter()
            .map(|e| match e {
                SidebarMenuEntry::Item { title, destructive, .. } => if destructive { format!("{title}!") } else { title },
                SidebarMenuEntry::Separator => "-".into(),
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(titles(tracker_menu(&tracker("fav", 1, true))), ["Info", "Rename", "-", "Remove from Favorites!"]);
    assert_eq!(titles(tracker_menu(&tracker("auto.org", 1, false))), ["Info", "Save as Favorite"]);
    assert!(tracker_menu(&tracker("public", 1, false)).is_empty() && tracker_menu(&tracker("other-private", 1, false)).is_empty());
    assert_eq!(titles(channel_menu(&channel("Team", 2, 1, 0, false))), ["Share…", "New Topic…", "Mute", "Rename…", "-", "Leave Channel…"]);
    let mut c = channel("News", 2, 1, 0, true);
    c.role = SyncRole::ReadWrite;
    assert_eq!(titles(channel_menu(&c)), ["New Topic…", "Unmute", "Rename…", "-", "Leave Channel…"]);
    c.role = SyncRole::ReadOnly;
    assert_eq!(titles(channel_menu(&c)), ["Unmute", "Rename…", "-", "Leave Channel…"]);
    assert!(matches!(channel_menu(&c)[0], SidebarMenuEntry::Item { command: SidebarCommand::Unmute, .. }));
}

// MARK: Structure and cadence

#[test]
fn the_model_asks_to_be_refreshed_only_while_it_can_be_seen() {
    let i = inputs(snap());
    let mut st = state(LibraryPlace::Downloads);
    assert_eq!(model(&i, &st).refresh_ms, None);
    st.visible = true;
    let m = model(&i, &st);
    assert_eq!((m.refresh_ms, m.refresh_leeway_ms), (Some(1_000), 200));
}

#[test]
fn the_signature_changes_with_the_structure_not_with_the_numbers() {
    let mut s = snap();
    s.downloading = vec![named("a", download(TransferState::Downloading))];
    s.queued = vec![named("q", download(TransferState::Queued))];
    let st = state(LibraryPlace::Downloads);
    let a = model(&inputs(s.clone()), &st);
    s.downloading[0].down_rate = 9_000;
    s.down_rate = 9_000;
    let b = model(&inputs(s.clone()), &st);
    assert_eq!(a.signature, b.signature);
    assert_ne!(list(&a).groups[0].rows[0].revision, list(&b).groups[0].rows[0].revision);
    let mut folded = state(LibraryPlace::Downloads);
    folded.folded.insert("downloads.next".into());
    assert_ne!(model(&inputs(s.clone()), &folded).signature, b.signature);
    s.downloading[0].state = TransferState::Paused;
    assert_ne!(model(&inputs(s), &st).signature, b.signature, "the paused row has other controls");
}

// MARK: The object on the mock library

#[test]
fn the_library_reads_the_mock_and_pages_its_groups() {
    let (core, _d) = mock();
    let lib = library(&core);
    lib.set_visible(true);
    let m = lib.model(NOW_MS);
    assert_eq!((m.place.clone(), m.page, m.refresh_ms), (LibraryPlace::Downloads, LibraryPage::List, Some(1_000)));
    assert_eq!(lib.snapshot().downloading.len(), core.snapshot().downloading.len());
    lib.select_place(LibraryPlace::Sharing);
    let m = lib.model(NOW_MS);
    let dormant = group(&m, "dormant");
    assert_eq!(dormant.rows.len(), 150, "a page");
    let total: u32 = dormant.count.as_deref().unwrap().parse().unwrap();
    assert_eq!(dormant.more.as_ref().map(|x| x.title.clone()), Some(format!("Show {} more…", total - 150)));
    lib.show_more(SeedGroup::Dormant);
    assert_eq!(group(&lib.model(NOW_MS), "dormant").rows.len(), total.min(300) as usize);
    // A tab switch starts on the first page again, unfiltered.
    lib.toggle_category(Category::Movie);
    assert_eq!(list(&lib.model(NOW_MS)).filter_chip.as_ref().map(|c| c.category), Some(Category::Movie));
    lib.select_place(LibraryPlace::Sharing);
    let m = lib.model(NOW_MS);
    assert_eq!((group(&m, "dormant").rows.len(), list(&m).filter_chip.is_none()), (150, true));
    lib.toggle_category(Category::Tv);
    lib.toggle_category(Category::Tv);
    assert!(list(&lib.model(NOW_MS)).filter_chip.is_none(), "the same pill again clears");
    lib.toggle_category(Category::Tv);
    lib.clear_category();
    assert!(list(&lib.model(NOW_MS)).filter_chip.is_none());
    // Every place builds.
    let tracker = core.snapshot().trackers.iter().find(|t| t.id == "tracker-z.cc").map(|t| t.id.clone()).unwrap();
    for place in [
        LibraryPlace::Downloads,
        LibraryPlace::FolderSync,
        LibraryPlace::Settings,
        LibraryPlace::Tracker { tracker_id: tracker.clone() },
    ] {
        lib.select_place(place.clone());
        assert_eq!(lib.model(NOW_MS).place, place);
    }
    assert!(!list(&lib.model(NOW_MS)).groups.is_empty());
}

#[test]
fn folds_are_remembered_per_page_and_every_tracker_shares_them() {
    let (core, _d) = mock();
    let lib = Library::new(core.clone(), vec!["sharing.popular".into()], Arc::new(Clock));
    lib.select_place(LibraryPlace::Sharing);
    assert_eq!(lib.toggle_fold("dormant".into()), ["sharing.dormant", "sharing.popular"]);
    assert!(group(&lib.model(NOW_MS), "dormant").folded);
    lib.select_place(LibraryPlace::Tracker { tracker_id: "tracker-z.cc".into() });
    assert_eq!(lib.toggle_fold("downloads".into()), ["sharing.dormant", "sharing.popular", "tracker.downloads"]);
    lib.select_place(LibraryPlace::Tracker { tracker_id: "tracker-y.net".into() });
    let m = lib.model(NOW_MS);
    assert!(list(&m).groups.iter().filter(|g| g.key == "downloads").all(|g| g.folded));
    assert_eq!(lib.toggle_fold("downloads".into()), ["sharing.dormant", "sharing.popular"]);
}

#[test]
fn a_place_that_disappears_falls_back() {
    let (core, _d) = mock();
    let lib = library(&core);
    // A deleted tracker profile: its tab goes to Sharing.
    lib.select_place(LibraryPlace::Tracker { tracker_id: "deleted-profile".into() });
    assert_eq!(lib.model(NOW_MS).place, LibraryPlace::Sharing);
    // A channel left (here or by its removal elsewhere): Downloads, and its flag goes.
    let c = core.chat_channels().first().map(|c| c.id.clone()).expect("the mock has channels");
    lib.select_place(LibraryPlace::Chat { channel_id: c.clone(), topic_id: None });
    assert_eq!(lib.model(NOW_MS).place, LibraryPlace::Chat { channel_id: c.clone(), topic_id: None });
    lib.with_state(|st| st.downloads_flag = true); // a new download while the user was busy
    core.backend.sync().chat_leave(&c).unwrap();
    let m = lib.model(NOW_MS);
    assert_eq!(m.place, LibraryPlace::Downloads);
    assert!(m.sidebar.places[0].flag.is_none());
}

#[test]
fn a_topic_shown_opens_its_channel_and_selecting_resets_the_tab() {
    let (core, _d) = mock();
    let lib = library(&core);
    let c = core.chat_channels().into_iter().find(|c| !core.chat_topics(c.id.clone()).is_empty()).expect("a channel with topics");
    let t = core.chat_topics(c.id.clone())[0].id.clone();
    lib.select_place(LibraryPlace::Chat { channel_id: c.id.clone(), topic_id: Some(t) });
    let m = lib.model(NOW_MS);
    assert!(m.sidebar.chats.channels.iter().find(|x| x.item.title == c.name).is_some_and(|x| x.open));
    lib.set_channel_open(c.id.clone(), false);
    lib.select_place(LibraryPlace::Downloads);
    assert!(lib.model(NOW_MS).sidebar.chats.channels.iter().all(|x| !x.open));
    // The Downloads flag stays until Downloads is shown; a tracker's Info until another place is.
    lib.select_place(LibraryPlace::Sharing);
    lib.with_state(|st| st.downloads_flag = true); // a new download while the user was busy
    assert!(lib.model(NOW_MS).sidebar.places[0].flag.is_some());
    lib.select_place(LibraryPlace::Downloads);
    assert!(lib.model(NOW_MS).sidebar.places[0].flag.is_none());
    lib.select_place(LibraryPlace::Tracker { tracker_id: "tracker-z.cc".into() });
    lib.set_tracker_info(true);
    assert_eq!(lib.model(NOW_MS).page, LibraryPage::TrackerInfo);
    lib.select_place(LibraryPlace::Tracker { tracker_id: "tracker-z.cc".into() });
    assert_eq!(lib.model(NOW_MS).page, LibraryPage::List);
    lib.note_direct_name("dev-x".into(), "Kari's iPhone".into());
    lib.select_place(LibraryPlace::Direct { device_id: "dev-x".into() });
    assert_eq!(lib.model(NOW_MS).title, "Kari's iPhone");
    // The menus come from what the model showed.
    assert_eq!(lib.sidebar_menu(LibraryPlace::Tracker { tracker_id: "tracker-z.cc".into() }).len(), 2);
    assert!(!lib.sidebar_menu(LibraryPlace::Chat { channel_id: c.id.clone(), topic_id: None }).is_empty());
    assert!(lib.sidebar_menu(LibraryPlace::Downloads).is_empty());
}

#[test]
fn the_day_has_its_year_only_when_it_is_not_this_one() {
    // 2026-12-31 23:30 UTC is 2027 already an hour east.
    let eve = 1_798_759_800;
    assert_eq!(local_day(&Clock, eve, NOW_S), format!("D{eve}"));
    assert_eq!(local_day(&Clock, NOW_S - 60 * 86_400, NOW_S), format!("D{}Y", NOW_S - 60 * 86_400));
    assert_eq!(local_year(&Clock, 0), 1970);
    assert_eq!(local_year(&Clock, 951_782_400), 2000, "2000-02-29");
}

/// The cost of one model with a thousand rows (`tools/sysguard/heavy.sh cargo test --release -p trs-ffi -- --ignored
/// a_thousand_rows --nocapture`). Prints; never asserts a time.
#[test]
#[ignore]
fn a_thousand_rows() {
    let mut s = snap();
    s.downloading = (0..400).map(|n| named(&format!("d{n}"), download(TransferState::Downloading))).collect();
    s.queued = (0..100).map(|n| named(&format!("q{n}"), download(TransferState::Queued))).collect();
    let seeds = SEED_GROUPS.iter().map(|&g| (g, (0..85).map(|n| seeding(&format!("{g:?}{n}"), g)).collect::<Vec<_>>())).collect::<Vec<_>>();
    let i = Inputs { sharing: Some(sharing(&seeds)), ..inputs(s) };
    for place in [LibraryPlace::Downloads, LibraryPlace::Sharing] {
        let st = state(place.clone());
        let start = std::time::Instant::now();
        let n = 50;
        for _ in 0..n {
            std::hint::black_box(model(&i, &st));
        }
        println!("{place:?}: {:?} per model", start.elapsed() / n);
    }
}
