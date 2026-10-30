use super::*;
use crate::library::tests::Clock;
use crate::row_commands::{PlayerOption, RowCommand};
use crate::row_text::tests::row;
use crate::types::{Category, FilePriority, Priority, RowQuery, SeedGroup, SeedingGoal, TorrentFile};
use crate::CoreOptions;

/// 2027-01-15 08:00 UTC (09:00 for [`Clock`], an hour east).
const NOW_S: u64 = 1_800_000_000;

fn tracker(state: &str) -> TrackerLine {
    TrackerLine {
        url: "https://tracker.example/announce?passkey=••••••".into(),
        tier: 0,
        status: "announced".into(),
        last_announce: None,
        next_announce: None,
        seeders: None,
        leechers: None,
        state: state.into(),
        failures: 0,
        retry_at: None,
        last_success: None,
        manual_ok_at: None,
    }
}

fn info() -> TorrentInfo {
    TorrentInfo {
        id: "a".into(),
        name: "a".into(),
        display_name: "a".into(),
        category: Category::Movie,
        size_bytes: 1_500_000_000,
        peers: 0,
        down_rate: 0,
        save_path: "/data/Movies/a".into(),
        priority: Priority::Normal,
        seeding_goal: "Rule default (ratio 1.0)".into(),
        idle_text: None,
        files_wanted: 1,
        files_total: 1,
        skipped_note: String::new(),
        rule_name: "Default".into(),
        tracker_name: "Public".into(),
        error: None,
        tracker_problem: None,
        tracker_halted: false,
        goal_is_custom: false,
        state: TransferState::Downloading,
        added_at: NOW_S - 3_600,
        finished_at: None,
        seeded_secs: 0,
        ratio: 0.27,
        uploaded_total: 270,
        downloaded_total: 1_000,
        piece_count: 0,
        piece_length: 0,
        info_hash: "0123abcd".into(),
        is_private: false,
        seeds_connected: 0,
        swarm_seeders: None,
        swarm_leechers: None,
        trackers: Vec::new(),
    }
}

fn card(row: &TransferRow, info: Option<&TorrentInfo>) -> InfoCardModel {
    card_with(row, info, &[], true, NOW_S)
}

fn card_with(row: &TransferRow, info: Option<&TorrentInfo>, play: &[CommandEntry], chooser: bool, now_s: u64) -> InfoCardModel {
    build(&CardInputs { row, info, play, chooser_available: chooser, now_s, clock: &Clock })
}

fn section<'a>(m: &'a InfoCardModel, title: &str) -> &'a InfoCardSection {
    m.sections.iter().find(|s| s.title == title).unwrap_or_else(|| panic!("no section {title}: {}", m.signature))
}

/// "Label=value" of every line of a section, a run in another colour than the value's own in brackets.
fn lines(s: &InfoCardSection) -> Vec<String> {
    s.rows
        .iter()
        .map(|r| {
            let v: String = r
                .value
                .iter()
                .map(|x| match x.tone {
                    InfoTone::Primary => x.text.clone(),
                    InfoTone::Secondary => format!("[{}]", x.text),
                    InfoTone::Problem => format!("{{{}}}", x.text),
                })
                .collect();
            format!("{}={v}", r.label)
        })
        .collect()
}

fn find<'a>(s: &'a InfoCardSection, label: &str) -> &'a InfoCardLine {
    s.rows.iter().find(|r| r.label == label).unwrap_or_else(|| panic!("no line {label}: {:?}", lines(s)))
}

fn sharing() -> TransferRow {
    let mut r = row("a", TransferState::Seeding);
    r.seed_group = Some(SeedGroup::Active);
    r
}

#[test]
fn a_download_shows_every_general_line_in_order() {
    let m = card(&row("a", TransferState::Downloading), Some(&info()));
    assert_eq!(m.sections.iter().map(|s| s.title.as_str()).collect::<Vec<_>>(), ["General", "Peers", "Rule and goal"]);
    assert_eq!(
        lines(section(&m, "General")),
        [
            "Title=a",
            "Category=Movie",
            "Size=1.5 GB",
            "State=Downloading",
            &format!("Added=DT{}", NOW_S - 3_600),
            "Ratio=0.27",
            "Uploaded=270 bytes",
            "Downloaded=1.0 KB",
            "Location=/data/Movies/a",
            "Files=1 of 1",
            "Info-hash=0123abcd",
            "Type=Public",
        ]
    );
    let g = section(&m, "General");
    assert_eq!(find(g, "Info-hash").style, InfoValueStyle::Code);
    assert_eq!(find(g, "Location").style, InfoValueStyle::Long);
    assert!(g.rows.iter().all(|r| r.selectable && r.menu.is_none()), "values can be copied");
}

#[test]
fn the_release_name_finished_seeding_time_last_active_and_pieces_show_when_they_apply() {
    let mut r = sharing();
    r.name = "A.2010.1080p.mkv".into();
    r.last_activity = Some(NOW_S - 300);
    r.ratio_goal = Some(1.0);
    let i = TorrentInfo { finished_at: Some(NOW_S - 60), seeded_secs: 7_200, piece_count: 360, piece_length: 4 << 20, is_private: true, state: TransferState::Seeding, ..info() };
    let g = lines(section(&card(&r, Some(&i)), "General"));
    assert_eq!(g[1], "Release=A.2010.1080p.mkv");
    for want in [&format!("Finished=DT{}", NOW_S - 60), "Seeding time=2 hours", "Last active=5 minutes ago", "Ratio=0.27 of 1.0", "Pieces=360 × 4 MB", "Type=Private", "State=Seeding"] {
        assert!(g.iter().any(|l| l == want), "{want} in {g:?}");
    }
    assert_eq!(find(section(&card(&r, Some(&i)), "General"), "Release").style, InfoValueStyle::Long);
    // A download has no "Last active" (it is a sharing figure).
    let mut d = row("a", TransferState::Downloading);
    d.last_activity = Some(NOW_S - 300);
    assert!(!lines(section(&card(&d, Some(&info())), "General")).iter().any(|l| l.starts_with("Last active")));
}

#[test]
fn files_say_what_was_skipped_and_offer_the_chooser_for_several_files() {
    let i = TorrentInfo { files_wanted: 3, files_total: 4, skipped_note: "sample skipped".into(), ..info() };
    let m = card(&row("a", TransferState::Downloading), Some(&i));
    let files = find(section(&m, "General"), "Files");
    assert_eq!(lines(section(&m, "General")).iter().find(|l| l.starts_with("Files")).unwrap(), "Files=3 of 4[ · sample skipped]");
    let choose = files.button.as_ref().expect("Choose…");
    assert_eq!((choose.title.as_str(), choose.help.as_str(), choose.command), ("Choose…", "Choose which files to download", InfoCommand::ChooseFiles));
    // Not for one file, nor when the engine cannot reopen the chooser.
    assert!(find(section(&card(&row("a", TransferState::Downloading), Some(&info())), "General"), "Files").button.is_none());
    let m = card_with(&row("a", TransferState::Downloading), Some(&i), &[], false, NOW_S);
    assert!(find(section(&m, "General"), "Files").button.is_none());
}

#[test]
fn the_location_is_shown_with_a_tilde_and_shows_the_files_in_the_file_manager() {
    let Some(home) = trs_platform::dirs::home() else { return };
    let i = TorrentInfo { save_path: home.join("Movies/Sprite Fright").display().to_string(), ..info() };
    let m = card(&row("a", TransferState::Downloading), Some(&i));
    let loc = find(section(&m, "General"), "Location");
    if cfg!(unix) && home.parent().is_some() {
        assert_eq!(loc.value[0].text, "~/Movies/Sprite Fright");
    }
    let b = loc.button.as_ref().unwrap();
    assert!(b.symbol_only && b.command == InfoCommand::Reveal && b.title == b.help);
}

#[cfg(target_os = "macos")]
#[test]
fn the_mac_says_finder() {
    let m = card(&row("a", TransferState::Downloading), Some(&info()));
    let loc = find(section(&m, "General"), "Location").button.clone().unwrap();
    assert_eq!((loc.title.as_str(), loc.help.as_str()), ("Show in Finder", "Show in Finder"));
    let b: Vec<(&str, &str)> = m.buttons.iter().map(|b| (b.title.as_str(), b.help.as_str())).collect();
    assert_eq!(
        b,
        [
            ("Show in Finder", "Show the downloaded files in Finder"),
            ("Remove…", "Remove this torrent from Seedwell; asks under its row first, and keeps the files"),
        ]
    );
}

#[test]
fn remove_asks_under_the_row_after_the_card_closes() {
    let m = card(&row("a", TransferState::Downloading), None);
    let [reveal, remove] = &m.buttons[..] else { panic!("{:?}", m.buttons) };
    assert!(reveal.command == InfoCommand::Reveal && !reveal.destructive && !reveal.closes_card && !reveal.symbol_only);
    assert!(remove.command == InfoCommand::Remove && remove.destructive && remove.closes_card);
}

#[test]
fn every_state_has_its_word() {
    let words: Vec<&str> = [
        TransferState::Checking,
        TransferState::Metadata,
        TransferState::Downloading,
        TransferState::Queued,
        TransferState::Paused,
        TransferState::Seeding,
        TransferState::LowPower,
        TransferState::Error,
        TransferState::MissingData,
        TransferState::Completed,
    ]
    .into_iter()
    .map(state_label)
    .collect();
    assert_eq!(
        words,
        ["Checking files", "Fetching metadata", "Downloading", "Waiting", "Paused", "Seeding", "Seeding (resting)", "Error", "Files missing", "Finished"]
    );
}

#[test]
fn a_piece_size_is_written_as_the_mac_writes_it() {
    for (bytes, want) in [
        (16_384, "16 KB"),
        (262_144, "256 KB"),
        (524_288, "512 KB"),
        (1 << 20, "1 MB"),
        (4 << 20, "4 MB"),
        (16 << 20, "16 MB"),
        (1_572_864, "1.5 MB"),
        (1_258_291, "1.2 MB"),
        (1_536, "2 KB"),
        (123_456, "121 KB"),
        (512, "512 bytes"),
        (1, "1 byte"),
        (1 << 30, "1 GB"),
        (1_610_612_736, "1.5 GB"),
    ] {
        assert_eq!(piece_size(bytes), want, "{bytes}");
    }
}

#[test]
fn a_refusal_is_red_and_waits_for_the_user() {
    let p = TrackerProblem {
        host: "tracker-z.cc".into(),
        message: "The tracker refused this torrent: “Unregistered torrent”.".into(),
        since: Some(NOW_S - 2_400),
        next_try_ms: None,
        temporary: false,
        reassurance: None,
    };
    let i = TorrentInfo { tracker_problem: Some(p), ..info() };
    let g = lines(section(&card(&row("a", TransferState::Downloading), Some(&i)), "General"));
    assert_eq!(
        g[1..5],
        [
            "Problem={The tracker refused this torrent: “Unregistered torrent”.}".to_owned(),
            "Tracker=tracker-z.cc".into(),
            format!("Since=T{}", NOW_S - 2_400),
            "Next try=[Not until you ask the tracker again]".into(),
        ]
    );
}

#[test]
fn a_passing_outage_is_plain_says_when_it_tries_again_and_that_the_files_are_fine() {
    let p = TrackerProblem {
        host: String::new(),
        message: "The tracker's server is down or overloaded (it answered 502 Bad Gateway).".into(),
        since: None,
        // Tomorrow: the weekday form.
        next_try_ms: Some((NOW_S + 86_400) * 1000),
        temporary: true,
        reassurance: Some("Your files are fine.".into()),
    };
    let i = TorrentInfo { tracker_problem: Some(p), error: Some("Disk full".into()), ..info() };
    let g = lines(section(&card(&row("a", TransferState::Downloading), Some(&i)), "General"));
    assert_eq!(
        g[1..5],
        [
            "Problem={Disk full}".to_owned(),
            "Problem=The tracker's server is down or overloaded (it answered 502 Bad Gateway).".into(),
            format!("Next try=W{}", NOW_S + 86_400),
            "=[Your files are fine.]".into(),
        ]
    );
}

#[test]
fn peers_say_how_many_are_seeds_and_where_known_peers_come_from() {
    let mut r = row("a", TransferState::Downloading);
    r.known_peers = 57;
    let i = TorrentInfo { peers: 8, seeds_connected: 3, swarm_seeders: Some(120), down_rate: 2_100_000, ..info() };
    let m = card(&r, Some(&i));
    let p = section(&m, "Peers");
    assert_eq!(lines(p), ["Connected=8 / 57 peers[ · 3 seeds, 5 leechers]", "Swarm (tracker)=120 seeders · ? leechers", "Speed=2.1 MB/s down"]);
    assert!(find(p, "Connected").help.as_deref().unwrap().contains(
        "tracker and incoming connections (private torrents; plus PEX when the tracker allows it); plus DHT and PEX for public ones"
    ));
    // Nobody connected, nothing reported, not downloading.
    let i = TorrentInfo { state: TransferState::Seeding, ..info() };
    assert_eq!(lines(section(&card(&sharing(), Some(&i)), "Peers")), ["Connected=0 / 0 peers", "Swarm (tracker)=[Not reported by the tracker]"]);
}

#[test]
fn one_peer_seed_or_leecher_is_said_in_the_singular() {
    let mut r = row("a", TransferState::Downloading);
    r.known_peers = 3;
    let i = TorrentInfo { peers: 2, seeds_connected: 1, swarm_seeders: Some(1), swarm_leechers: Some(1), down_rate: 1_000, ..info() };
    let m = card(&r, Some(&TorrentInfo { peers: 1, ..i.clone() }));
    assert_eq!(m.subtitle, "Movie · 100 bytes · 1.0 KB/s from 1 peer");
    let m = card(&r, Some(&i));
    let p = section(&m, "Peers");
    assert_eq!(lines(p)[..2], ["Connected=2 / 3 peers[ · 1 seed, 1 leecher]", "Swarm (tracker)=1 seeder · 1 leecher"]);
    let t = TrackerLine { seeders: Some(1), leechers: Some(0), ..tracker("working") };
    assert_eq!(tracker_detail(&t, NOW_S), "Announced · 1 seeder · 0 leechers");
    assert_eq!(tracker_detail(&TrackerLine { leechers: Some(1), ..tracker("working") }, NOW_S), "Announced · ? seeders · 1 leecher");
}

#[test]
fn the_header_names_the_category_the_size_and_the_rate_or_the_tracker() {
    let mut r = row("a", TransferState::Downloading);
    r.size_bytes = 1_500_000_000;
    r.file_ext = "mkv".into();
    let i = TorrentInfo { peers: 8, down_rate: 2_100_000, ..info() };
    let m = card(&r, Some(&i));
    assert_eq!((m.title.as_str(), m.subtitle.as_str(), m.icon_ext.as_str(), m.linked), ("a", "Movie · 1.5 GB · 2.1 MB/s from 8 peers", "mkv", false));
    let mut s = sharing();
    s.size_bytes = 1_500_000_000;
    let i = TorrentInfo { tracker_name: "Tracker Z".into(), ..info() };
    assert_eq!(card(&s, Some(&i)).subtitle, "Movie · 1.5 GB · Tracker Z");
    // A queued torrent, or one that could not be read: category and size only, and nothing below the header.
    assert_eq!(card(&row("a", TransferState::Queued), Some(&info())).subtitle, "Movie · 100 bytes");
    let m = card(&s, None);
    assert_eq!(m.subtitle, "Movie · 1.5 GB");
    assert!(m.sections.is_empty());
    let mut d = row("d", TransferState::Completed);
    d.kind = TransferKind::Direct;
    assert!(card(&d, None).linked);
}

#[test]
fn a_tracker_says_what_it_is_doing_from_absolute_times() {
    // Ported from the app's TrackerLiveTests.
    let t = TrackerLine { failures: 2, retry_at: Some(NOW_S + 540), ..tracker("backoff") };
    assert_eq!(tracker_live(&t, NOW_S, &Clock), "Retrying tracker in 9 min (attempt 3)");
    assert_eq!(tracker_live(&t, NOW_S + 600, &Clock), "Retrying tracker now…");
    assert_eq!(tracker_live(&tracker("backoff"), NOW_S, &Clock), "Tracker not responding");
    let t = TrackerLine { next_announce: Some(NOW_S + 1_800), ..tracker("working") };
    assert_eq!(tracker_live(&t, NOW_S, &Clock), format!("Tracker OK · next announce T{}", NOW_S + 1_800));
    let t = TrackerLine { next_announce: Some(NOW_S + 86_400), ..tracker("warning") };
    assert_eq!(tracker_live(&t, NOW_S, &Clock), format!("Tracker OK · next announce W{}", NOW_S + 86_400));
    assert_eq!(tracker_live(&tracker("working"), NOW_S, &Clock), "Tracker OK");
    assert_eq!(tracker_live(&tracker("waiting"), NOW_S, &Clock), "Announcing to tracker.example…");
    assert_eq!(tracker_live(&TrackerLine { url: "no scheme".into(), ..tracker("waiting") }, NOW_S, &Clock), "Announcing to the tracker…");
    assert_eq!(tracker_live(&tracker("halted"), NOW_S, &Clock), "Announcing stopped: the tracker refused this torrent. It is asked again when you ask.");
}

#[test]
fn the_trackers_minimum_interval_offers_announce_anyway() {
    let ok = TrackerLine { manual_ok_at: Some(NOW_S + 300), ..tracker("working") };
    assert_eq!(tracker_allowed_at(&ok, NOW_S), Some(NOW_S + 300));
    assert_eq!(tracker_allowed_at(&TrackerLine { manual_ok_at: Some(NOW_S - 1_000), ..tracker("working") }, NOW_S), None);
    let i = TorrentInfo { trackers: vec![tracker("working"), ok], ..info() };
    let m = card(&row("a", TransferState::Downloading), Some(&i));
    let t = &section(&m, "Trackers").trackers;
    assert!(t[0].allowed.is_none() && t[0].announce_anyway.is_none());
    assert_eq!(t[1].allowed.as_deref(), Some(format!("Tracker allows the next announce at T{}", NOW_S + 300).as_str()));
    let b = t[1].announce_anyway.as_ref().unwrap();
    assert_eq!((b.title.as_str(), b.command), ("Announce anyway", InfoCommand::AnnounceAnyway));
    assert_eq!(b.help, "Ask now, ignoring the wait the tracker set. Allowed once per tracker per 5 minutes.");
    assert_eq!((t[0].index, t[1].index), (0, 1));
    assert_eq!((t[1].copy.title.as_str(), t[1].copy.help.as_str(), t[1].copy.command), ("Copy URL", "Copy URL", InfoCommand::CopyTrackerUrl));
    assert_eq!(t[1].url, "https://tracker.example/announce?passkey=••••••");
}

#[test]
fn a_trackers_line_says_its_status_when_it_was_asked_and_what_it_reported() {
    let t = TrackerLine { last_announce: Some(NOW_S - 300), next_announce: Some(NOW_S + 1_500), seeders: Some(120), leechers: Some(30), ..tracker("working") };
    assert_eq!(tracker_detail(&t, NOW_S), "Announced · last 5 minutes ago · next in 25 min · 120 seeders · 30 leechers");
    let t = TrackerLine { status: "failed: timed out".into(), leechers: Some(3), ..tracker("backoff") };
    assert_eq!(tracker_detail(&t, NOW_S), "Failed: timed out · ? seeders · 3 leechers");
}

#[test]
fn ask_tracker_again_is_offered_when_the_trackers_failed_or_halted() {
    let i = TorrentInfo { trackers: vec![tracker("working")], ..info() };
    let s = section(&card(&row("a", TransferState::Downloading), Some(&i)), "Trackers").clone();
    assert!(s.ask_again.is_none() && s.ask_again_note.is_none() && s.rows.is_empty());
    let halted = TorrentInfo { tracker_halted: true, ..i.clone() };
    let s = section(&card(&row("a", TransferState::Downloading), Some(&halted)), "Trackers").clone();
    let b = s.ask_again.unwrap();
    assert_eq!((b.title.as_str(), b.command), ("Ask Tracker Again", InfoCommand::AskTrackerAgain));
    assert_eq!(b.help, "Ask the tracker again now. Seedwell never does this by itself after a tracker said no, and asks at most once a minute.");
    assert_eq!(s.ask_again_note.as_deref(), Some("Announcing stopped until you ask."));
    // A dead seed can ask again (the row says so); the trackers did not all halt: no note.
    let mut dead = sharing();
    dead.seed_group = Some(SeedGroup::Dead);
    let s = section(&card(&dead, Some(&i)), "Trackers").clone();
    assert!(s.ask_again.is_some() && s.ask_again_note.is_none());
    // No trackers (a magnet on DHT): no section.
    assert!(!card(&row("a", TransferState::Downloading), Some(&info())).sections.iter().any(|s| s.title == "Trackers"));
}

#[test]
fn the_goal_menu_is_the_row_menus_and_names_the_goal_in_force() {
    let mut r = sharing();
    r.ratio_goal = Some(3.0);
    let i = TorrentInfo { seeding_goal: "Seed to ratio 3.0".into(), goal_is_custom: true, rule_name: "Custom".into(), idle_text: Some("Uploaded in the last 150 h: 0.3 GB (stops below 1 GB)".into()), ..info() };
    let s = section(&card(&r, Some(&i)), "Rule and goal").clone();
    assert_eq!(lines(&s), ["Rule=Custom", "Then=", "Inactive=Uploaded in the last 150 h: 0.3 GB (stops below 1 GB)"]);
    let then = find(&s, "Then");
    assert!(!then.selectable);
    let menu = then.menu.as_ref().unwrap();
    assert_eq!((menu.title.as_str(), menu.help.as_str()), ("Seed to ratio 3.0", "Choose what happens when this torrent is done downloading"));
    assert_eq!(menu.entries, seeding_goal_commands(r.clone(), Some(i.clone())));
    let checked: Vec<&str> = menu.entries.iter().filter_map(|e| match e {
        CommandEntry::Item { title, checked: true, .. } => Some(title.as_str()),
        _ => None,
    }).collect();
    assert_eq!(checked, ["Ratio 3.0"]);
    assert!(menu.entries.contains(&CommandEntry::Item { command: RowCommand::SetSeedingGoal { goal: SeedingGoal::Forever }, title: "Seed Forever".into(), checked: false, enabled: true, destructive: false, shortcut: None }));
}

#[test]
fn play_is_a_button_and_pop_ups_with_their_help() {
    let mut r = row("a", TransferState::Downloading);
    r.has_video = true;
    let video = |index: u32, name: &str| TorrentFile {
        index,
        path: name.into(),
        name: name.into(),
        length: 10,
        done: 0,
        uploaded: 0,
        wanted: true,
        priority: FilePriority::Normal,
        is_video: true,
        is_mp4_family: false,
        disk_path: String::new(),
    };
    let player = PlayerOption { id: "vlc".into(), name: "VLC".into(), runs: true, mp4_only: false };
    let one = play_commands(r.clone(), PlayOffer { videos: vec![video(0, "a.mkv")], players: vec![player.clone()] });
    let m = card_with(&r, Some(&info()), &one, true, NOW_S);
    let play = find(section(&m, "General"), "Play");
    assert!(!play.selectable && play.value.is_empty());
    let helps: Vec<&str> = play.play_controls.iter().map(|c| c.help.as_str()).collect();
    assert_eq!(helps, ["Play with Play", "Choose what to play"]);
    assert!(matches!(&play.play_controls[0].entry, CommandEntry::Item { command: RowCommand::Play { file_index: Some(0), player_id: None }, .. }));
    // Nothing to play: no Play line.
    assert!(!lines(section(&card(&r, Some(&info())), "General")).iter().any(|l| l.starts_with("Play")));
}

#[test]
fn an_announce_urls_host_is_read_as_the_mac_reads_it() {
    for (url, host) in [
        ("https://tracker.example/announce?passkey=••••••", "tracker.example"),
        ("udp://tracker.opentrackr.org:1337/announce", "tracker.opentrackr.org"),
        ("http://user:pw@t.example:80/a", "t.example"),
        ("https://t.example/••••••/announce", "t.example"),
        ("http://[::1]:8080/announce", "::1"),
        ("HTTP://Tracker.EXAMPLE/announce", "Tracker.EXAMPLE"),
        ("tracker.example/announce", ""),
        ("https://t.example:6969", "t.example"),
        ("https://t.example?x=1", "t.example"),
        ("wss://t.example#frag", "t.example"),
        ("", ""),
    ] {
        assert_eq!(url_host(url), host, "{url}");
    }
}

#[test]
fn the_card_reads_the_torrent_when_it_opens_and_when_asked_then_ticks_every_second() {
    let dir = tempfile::tempdir().unwrap();
    let core = Core::new(CoreOptions { config_dir: dir.path().display().to_string(), mock: true, allow_loopback_trackers: false, forbidden_root: None }).unwrap();
    let s = core.snapshot();
    let r = s.downloading.first().unwrap().clone();
    let c = GetInfoCard::new(core.clone(), r.clone(), Arc::new(Clock), true, None);
    let m = c.model(r.clone(), NOW_S * 1000);
    assert_eq!((m.row_id.as_str(), m.refresh_ms, m.refresh_leeway_ms), (r.id.as_str(), 1_000, 200));
    assert_eq!(section(&m, "General").rows[0].value[0].text, r.display_name);
    let goal = || section(&c.model(r.clone(), NOW_S * 1000), "Rule and goal").rows[1].menu.clone().unwrap().title;
    let before = goal();
    assert_ne!(before, "Seed to ratio 3.0");
    core.set_seeding_goal(vec![r.id.clone()], SeedingGoal::Ratio { ratio: 3.0 });
    assert_eq!(goal(), before, "nothing is read for a model");
    c.reload();
    assert_eq!(goal(), "Seed to ratio 3.0");
    // Gone: the header only.
    core.remove(vec![r.id.clone()], false);
    c.reload();
    assert!(c.model(r, NOW_S * 1000).sections.is_empty());
}

#[test]
fn an_open_card_reads_the_torrent_again_two_seconds_after_it_showed_it() {
    let dir = tempfile::tempdir().unwrap();
    let core = Core::new(CoreOptions { config_dir: dir.path().display().to_string(), mock: true, allow_loopback_trackers: false, forbidden_root: None }).unwrap();
    let r = core.snapshot().downloading.first().unwrap().clone();
    let c = GetInfoCard::new(core.clone(), r.clone(), Arc::new(Clock), true, None);
    let t0 = NOW_S * 1000;
    assert_eq!(c.model(r.clone(), t0).reload_due_ms, 2_000, "the read made at the opening counts from the first model");
    assert_eq!(c.model(r.clone(), t0 + 1_500).reload_due_ms, 500);
    assert_eq!(c.model(r.clone(), t0 + 2_500).reload_due_ms, 0, "due");
    // A clock that went back never waits longer than one interval.
    assert_eq!(c.model(r.clone(), t0 - 60_000).reload_due_ms, 2_000);
    // What changed meanwhile shows after the read.
    let goal = |m: InfoCardModel| m.sections.iter().find(|s| s.title == "Rule and goal").unwrap().rows[1].menu.clone().unwrap().title;
    core.set_seeding_goal(vec![r.id.clone()], SeedingGoal::Ratio { ratio: 3.0 });
    assert_ne!(goal(c.model(r.clone(), t0 + 2_500)), "Seed to ratio 3.0");
    c.reload();
    let m = c.model(r.clone(), t0 + 2_600);
    assert_eq!((goal(m.clone()), m.reload_due_ms), ("Seed to ratio 3.0".to_owned(), 2_000));
}

#[test]
fn the_mock_dates_every_torrent_s_adding_in_recent_years_not_in_1970() {
    let dir = tempfile::tempdir().unwrap();
    let core = Core::new(CoreOptions { config_dir: dir.path().display().to_string(), mock: true, allow_loopback_trackers: false, forbidden_root: None }).unwrap();
    let s = core.snapshot();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let all = RowQuery { group: None, tracker_id: None, text: String::new(), category: None, offset: 0, limit: u32::MAX };
    let rows: Vec<_> = s.downloading.into_iter().chain(s.queued).chain(s.recent).chain(core.sharing_rows(all)).collect();
    assert!(rows.iter().any(|r| r.finished_at.is_none() && r.state == TransferState::Downloading), "the mock has downloads");
    assert!(rows.iter().any(|r| r.finished_at.is_none() && r.state != TransferState::Downloading), "and seeds without a finish time");
    for r in rows {
        let added = core.info(r.id.clone()).unwrap().added_at;
        assert!(added <= now && now - added < 10 * 365 * 86_400, "{}: added {added}", r.display_name);
    }
}

#[test]
fn the_signature_follows_the_lines_not_the_values() {
    let r = row("a", TransferState::Downloading);
    let a = card_with(&r, Some(&info()), &[], true, NOW_S);
    let b = card_with(&r, Some(&TorrentInfo { ratio: 2.0, ..info() }), &[], true, NOW_S + 5);
    assert_eq!(a.signature, b.signature);
    let c = card_with(&r, Some(&TorrentInfo { finished_at: Some(NOW_S), ..info() }), &[], true, NOW_S);
    assert_ne!(a.signature, c.signature);
}
