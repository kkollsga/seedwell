use super::*;
use crate::favorite_editor::tests::{editor, profile};
use crate::host::host_windows_all;

fn line(url: &str, status: &str, last: Option<u64>, next: Option<u64>) -> TrackerLine {
    TrackerLine {
        url: url.into(),
        tier: 0,
        status: status.into(),
        last_announce: last,
        next_announce: next,
        seeders: None,
        leechers: None,
        state: "working".into(),
        failures: 0,
        retry_at: None,
        last_success: None,
        manual_ok_at: None,
    }
}

fn summary(id: &str, name: &str, favorite: bool, private: bool) -> TrackerSummary {
    TrackerSummary { id: id.into(), name: name.into(), count: 3, favorite, hidden: false, hide_only_seeding: false, private }
}

fn stats() -> TrackerStats {
    TrackerStats { lifetime_down: 0, lifetime_up: 0, torrents_completed: 61, files_downloaded: 0, seed_secs: 0 }
}

fn read(id: &str) -> TrackerInfoRead {
    TrackerInfoRead {
        tracker_id: id.into(),
        hosts: vec!["tracker-y.net".into()],
        sharing: 120,
        downloading: 1,
        busiest: Some(TrackerBusiest { name: "Royalty Free Vol. 3".into(), up_rate: 433_000 }),
        last_announce: Some(1_000),
        interval_secs: Some(1_800),
        problem: None,
        sampled: 2,
        pex_allowed: false,
        pex_master: true,
        stats: stats(),
        daily: vec![DailyStat { days_ago: 8, down_bytes: 5, up_bytes: 5 }, DailyStat { days_ago: 1, down_bytes: 1_000, up_bytes: 2_000 }],
    }
}

fn input(tracker: TrackerSummary, read: Option<TrackerInfoRead>) -> TrackerInfoInput {
    TrackerInfoInput { tracker, read, down: 0, up: 2_100_000, now_unix_s: 1_180, host: host_windows_all() }
}

fn titles(b: &[TrackerInfoButton]) -> Vec<&str> {
    b.iter().map(|b| b.title.as_str()).collect()
}

#[test]
fn hosts_are_read_from_masked_urls() {
    assert_eq!(host_of("https://Tracker.Example.org:443/a/••••••/announce"), "tracker.example.org");
    assert_eq!(host_of("udp://[::1]:6969/announce"), "::1");
    assert_eq!(host_of("http://127.0.0.1:7101/announce?x=1"), "127.0.0.1");
    assert_eq!(host_of("tracker.example.org"), "tracker.example.org");
}

#[test]
fn a_line_belongs_by_host_pattern_or_domain() {
    let hosts = |v: &[&str]| v.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
    assert!(belongs("http://bt.debian.org/announce", &[], Some("debian.org")));
    assert!(!belongs("http://notdebian.org/announce", &[], Some("debian.org")));
    assert!(belongs("http://t.example.org/a/••••••/announce", &hosts(&["t.example.org", "u.example.org/announce"]), None));
    assert!(belongs("http://x.example.org/announce", &hosts(&["*.example.org"]), None));
    assert!(!belongs("http://other.org/announce", &hosts(&["t.example.org"]), None));
}

#[test]
fn the_facts_are_the_newest_announce_the_mean_interval_and_the_first_failure() {
    let f = summarize(&[
        line("a", "announced", Some(100), Some(1_900)),
        line("b", "announced", Some(300), Some(2_100)),
        line("c", "failed: unregistered torrent", Some(50), None),
        line("d", "Tracker error", None, None),
    ]);
    assert_eq!(f, Facts { last_announce: Some(300), interval_secs: Some(1_800), problem: Some("failed: unregistered torrent".into()), sampled: 4 });
    assert_eq!(summarize(&[]), Facts::default());
}

#[test]
fn spans_round_to_their_unit() {
    assert_eq!((span(45), span(1_800), span(7_200), span(259_200)), ("45 s".into(), "30 min".into(), "2 h".into(), "3 days".into()));
    assert_eq!(ago(100, 100), "just now");
    assert_eq!(ago(100, 280), "3 min ago");
}

#[test]
fn the_header_sums_the_last_seven_days() {
    let h = tracker_header_model(stats(), read("x").daily, 0, 2_100_000);
    let line: Vec<String> = h.stats.iter().map(|s| format!("{}|{}|{}", s.value, s.caption, s.sub.as_deref().unwrap_or(""))).collect();
    assert_eq!(line[2], format!("{}|Uploaded|Last 7 days", d::bytes(2_000)));
    assert_eq!(line[4], format!("{}|Ratio|Last 7 days", d::ratio(2.0)));
    assert_eq!(line[5], "61|Torrents completed|");
    assert_eq!(h.stats[1].value, d::live_rate(2_100_000, "↑"));
    assert_eq!(h.chart.len(), 2);
    let quiet = tracker_header_model(stats(), vec![DailyStat { days_ago: 0, down_bytes: 9, up_bytes: 0 }], 0, 0);
    assert!(quiet.chart.is_empty(), "no upload, no sparkline");
    assert_eq!(quiet.stats[4].value, d::ratio(0.0));
}

#[test]
fn an_automatic_tracker_shows_its_hosts_and_how_to_get_settings() {
    let (_, e) = editor(vec![]);
    let p = e.tracker_info_page(input(summary("tracker-y.net", "tracker-y.net", false, false), Some(read("tracker-y.net"))));
    assert_eq!((p.title.as_str(), p.subtitle.as_str(), p.favorite), ("tracker-y.net", "Info", false));
    assert_eq!((p.back.title.as_str(), p.back.help.as_str()), ("Torrents", "Back to this tracker's torrents"));
    let now: Vec<String> = p.now.iter().map(|f| format!("{}: {}", f.label, f.value)).collect();
    assert_eq!(now, ["Torrents: 120 sharing · 1 downloading", &format!("Busiest now: Royalty Free Vol. 3 · ↑ {}", d::rate(433_000)), "Tracker: Working", "Last announce: 3 min ago", "Announce interval: about 30 min"]);
    assert!(p.pex.is_none(), "not private");
    assert!(p.limits.rows.is_empty() && p.limits.note == "Save as Favorite to set limits");
    assert!(p.location.record.is_none() && p.location.note.as_deref() == Some("Save as Favorite to choose folders"));
    assert_eq!(p.addresses.rows.iter().map(|r| r.text.as_str()).collect::<Vec<_>>(), ["tracker-y.net"]);
    assert!(p.addresses.rows[0].buttons.is_empty() && p.addresses.add.is_none());
    assert_eq!(p.addresses.note.as_deref(), Some("Save as Favorite to edit"));
    assert_eq!(p.refresh_ms, 10_000);
    // Before the first read, and a read of another tracker, say nothing yet.
    let p = e.tracker_info_page(input(summary("tracker-y.net", "tracker-y.net", false, false), Some(read("other"))));
    assert!(p.header.is_none());
    assert_eq!(p.now.iter().map(|f| f.value.as_str()).collect::<Vec<_>>(), ["0 sharing · 0 downloading", "Not announced yet"]);
}

#[test]
fn a_failing_tracker_says_why_in_red() {
    let (_, e) = editor(vec![]);
    let mut r = read("t");
    r.problem = Some("failed: unregistered".into());
    let p = e.tracker_info_page(input(summary("t", "T", false, false), Some(r)));
    let tracker = p.now.iter().find(|f| f.label == "Tracker").unwrap();
    assert_eq!((tracker.value.as_str(), tracker.error), ("failed: unregistered", true));
}

#[test]
fn a_private_tracker_has_peer_exchange_unless_settings_turned_it_off() {
    let (store, e) = editor(vec![]);
    let p = e.tracker_info_page(input(summary("t", "T", false, true), Some(read("t"))));
    let x = p.pex.unwrap();
    assert_eq!((x.on, x.enabled, x.help.as_str(), x.note_is_reason), (false, true, "Off, click to turn on", false));
    assert!(x.note.starts_with("Only turn this on"));
    let mut r = read("t");
    r.pex_master = false;
    r.pex_allowed = true;
    let x = e.tracker_info_page(input(summary("t", "T", false, true), Some(r))).pex.unwrap();
    assert_eq!((x.enabled, x.help.as_str(), x.note.as_str(), x.note_is_reason), (false, "Peer exchange is off in Settings › Network", "Peer exchange is off in Settings › Network", true));
    e.tracker_info_run("t".into(), TrackerInfoCommand::SetPex { allow: true });
    assert_eq!(*store.pex.lock().unwrap(), [("t".to_string(), true)]);
}

#[test]
fn a_favorite_has_limits_folders_and_editable_addresses() {
    let mut fav = profile("z", "Tracker-z", &["tracker-z.cc"]);
    fav.down_limit = Some(5_000_000);
    let (_, e) = editor(vec![fav]);
    let p = e.tracker_info_page(input(summary("z", "Tracker-z", true, true), Some(read("z"))));
    assert!(p.favorite);
    let limits: Vec<(&str, u64, &str)> = p.limits.rows.iter().map(|l| (l.label.as_str(), l.limit, l.line.as_str())).collect();
    assert_eq!(limits, [("Download", 5_000_000, "0 KB/s / 5 MB/s"), ("Upload", 0, d::rate(2_100_000).as_str())]);
    assert_eq!(p.limits.note, "One cap for all torrents on Tracker-z together. The global limits still apply; the lower one wins.");
    assert!(p.location.record.is_some() && p.location.buttons.is_empty());
    let row = &p.addresses.rows[0];
    assert_eq!(titles(&row.buttons), ["Edit this address", "Remove this address (it is the only one: removes the favorite)"]);
    assert_eq!(p.addresses.add.as_ref().map(|b| b.symbol), Some(Some(TrackerInfoSymbol::Add)));
    assert_eq!(p.addresses.note, None);
    // A host without the editing shows them, not their pens.
    let mut no_edit = input(summary("z", "Tracker-z", true, true), Some(read("z")));
    no_edit.host = vec![HostWindow::TrackerInfo];
    let p = e.tracker_info_page(no_edit);
    assert!(p.addresses.rows[0].buttons.is_empty() && p.addresses.add.is_none());
}

#[test]
fn an_address_being_edited_is_a_field_with_its_hint_and_a_new_one_comes_last() {
    let (_, e) = editor(vec![profile("z", "Z", &["a.org", "b.org"])]);
    e.tracker_info_run("z".into(), TrackerInfoCommand::EditAddress { index: 1 });
    e.set_text("bad address!".into());
    e.commit();
    let p = e.tracker_info_page(input(summary("z", "Z", true, false), None));
    assert!(p.addresses.rows[0].field.is_none());
    assert_eq!(titles(&p.addresses.rows[0].buttons), ["Edit this address", "Remove this address"]);
    let f = p.addresses.rows[1].field.as_ref().unwrap();
    assert_eq!((f.text.as_str(), f.prompt.as_str()), ("bad address!", ADDRESS_PROMPT));
    assert!(f.hint.as_deref().is_some_and(|h| h.starts_with("Not a tracker address")));
    assert!(p.addresses.rows[1].buttons.is_empty());
    assert_eq!(p.location.hint, None, "the field's hint is not the folders'");
    e.cancel();
    e.tracker_info_run("z".into(), TrackerInfoCommand::AddAddress);
    let p = e.tracker_info_page(input(summary("z", "Z", true, false), None));
    assert_eq!(p.addresses.adding.map(|f| f.target), Some(FavoriteTarget::Url { tracker_id: "z".into(), index: None }));
    e.set_text("https://c.org/announce?passkey=1".into());
    assert!(e.commit().changed);
    let p = e.tracker_info_page(input(summary("z", "Z", true, false), None));
    assert_eq!(p.addresses.rows.len(), 3);
    assert_eq!(p.addresses.note.as_deref(), Some(crate::favorite_editor::PASSKEY_NOTE));
}

#[test]
fn changed_folders_offer_revert_and_save() {
    let (store, e) = editor(vec![profile("z", "Z", &["a.org"])]);
    let mut draft = store.profiles.lock().unwrap()[0].clone();
    draft.name = "renamed in the draft".into();
    e.set_folder_draft("z".into(), draft.clone());
    let p = e.tracker_info_page(input(summary("z", "Z", true, false), None));
    assert!(p.location.buttons.is_empty(), "the same folders");
    draft.incomplete_dir = Some("/Volumes/X/inc".into());
    e.set_folder_draft("z".into(), draft);
    let p = e.tracker_info_page(input(summary("z", "Z", true, false), None));
    assert_eq!(titles(&p.location.buttons), ["Revert", "Save"]);
    assert!(p.location.buttons[1].prominent);
    assert_eq!(p.location.record.as_ref().and_then(|r| r.incomplete_dir.as_deref()), Some("/Volumes/X/inc"));
    assert_eq!(p.location.warnings.len(), 1, "the draft's warnings");
    // A save the core refuses says why under the folders.
    *store.fail.lock().unwrap() = true;
    e.tracker_info_run("z".into(), TrackerInfoCommand::SaveFolders);
    assert_eq!(e.tracker_info_page(input(summary("z", "Z", true, false), None)).location.hint.as_deref(), Some("disk full"));
    e.set_folder_draft("z".into(), store.profiles.lock().unwrap()[0].clone());
    e.tracker_info_run("z".into(), TrackerInfoCommand::RevertFolders);
    assert!(e.tracker_info_page(input(summary("z", "Z", true, false), None)).location.buttons.is_empty());
}

#[test]
fn the_removal_question_shows_on_its_own_trackers_page() {
    let (store, e) = editor(vec![profile("z", "Z", &["a.org"]), profile("y", "Y", &["y.org"])]);
    e.tracker_info_run("z".into(), TrackerInfoCommand::RemoveAddress { index: 0 });
    assert!(e.tracker_info_page(input(summary("y", "Y", true, false), None)).removal.is_none());
    let q = e.tracker_info_page(input(summary("z", "Z", true, false), None)).removal.unwrap();
    assert!(q.message.starts_with("That is the only address."));
    let fx = e.tracker_info_run("z".into(), TrackerInfoCommand::ConfirmRemoval);
    assert_eq!(fx.gone.as_deref(), Some("z"));
    assert_eq!(store.profiles.lock().unwrap().len(), 1);
}

#[test]
fn the_read_counts_a_mock_trackers_torrents() {
    let dir = tempfile::tempdir().unwrap();
    let core = Core::new(crate::CoreOptions { config_dir: dir.path().display().to_string(), mock: true, allow_loopback_trackers: false, forbidden_root: None }).unwrap();
    let r = core.tracker_info_read(summary("tracker-y.net", "tracker-y.net", false, true));
    assert_eq!(r.hosts, ["tracker-y.net"]);
    assert!(r.sharing > 0 && r.sampled > 0, "{r:?}");
    assert_eq!(r.daily.len(), 30);
    assert_eq!(r.problem, None);
}
