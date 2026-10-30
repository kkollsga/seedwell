//! Loading, saving, backing up and hot-reloading the three TOML documents.

use crate::common;

use std::fs;
use std::sync::mpsc;
use std::time::Duration;

use common::{TempDir, abs, win};
use trs_state::doc::{BACKUPS_KEEP, DocStore, Document, LoadSource, PollEvent, Poller, spawn_watcher};
use trs_state::rules::{Priority, Rule, RuleSet};
use trs_state::trackers::{Profile, TrackerProfiles};
use trs_state::{Config, DocError};

fn names(dir: &std::path::Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    v.sort();
    v
}

#[test]
fn missing_file_is_created_with_defaults_and_is_valid_toml() {
    let dir = TempDir::new("create");
    let store = DocStore::<Config>::new(dir.path());
    let l = store.load();
    assert_eq!(l.source, LoadSource::Created);
    assert!(l.error.is_none() && l.warnings.is_empty());
    assert_eq!(l.value, Config::default());
    let again = store.load();
    assert_eq!(again.source, LoadSource::File);
    assert_eq!(again.value, l.value);
    let text = fs::read_to_string(store.path()).unwrap();
    assert!(text.starts_with("# Seedwell settings"), "{text}");
    // Struct order, not alphabetical: version first, tables in tab order.
    let pos = |needle: &str| text.find(needle).unwrap_or_else(|| panic!("{needle} missing in\n{text}"));
    assert!(pos("version =") < pos("[general]") && pos("[general]") < pos("[transfers]") && pos("[transfers]") < pos("[folders]"));
}

#[test]
fn save_rotates_backups_and_keeps_the_last_five() {
    let dir = TempDir::new("rotate");
    let store = DocStore::<Config>::new(dir.path());
    let mut c = store.load().value;
    let mut seen = Vec::new();
    for port in 20_000..20_009u16 {
        c.network.listen_port = port;
        store.save(&c).unwrap();
        seen.push(port);
        // Identical content again is a no-op: no extra backup.
        store.save(&c).unwrap();
    }
    let bak = DocStore::<Config>::new(dir.path()).path().with_file_name("config.toml.bak");
    let bak_cfg = Config::parse_str(&fs::read_to_string(&bak).unwrap()).unwrap().value;
    assert_eq!(bak_cfg.network.listen_port, 20_007, ".bak is the version before the last save");
    let backups = store.backups();
    assert_eq!(backups.len(), BACKUPS_KEEP, "{backups:?}");
    // Newest first, each a valid config of an earlier save.
    let ports: Vec<u16> = backups
        .iter()
        .map(|p| Config::parse_str(&fs::read_to_string(p).unwrap()).unwrap().value.network.listen_port)
        .collect();
    assert_eq!(ports, [20_007, 20_006, 20_005, 20_004, 20_003]);
    assert!(names(dir.path()).iter().all(|n| !n.contains(".tmp")), "no temp files left: {:?}", names(dir.path()));
}

#[test]
fn invalid_edit_falls_back_to_last_known_good_and_reports() {
    let dir = TempDir::new("fallback");
    let store = DocStore::<Config>::new(dir.path());
    let mut c = store.load().value;
    c.network.listen_port = 40_000;
    store.save(&c).unwrap();
    c.network.listen_port = 41_000;
    store.save(&c).unwrap(); // .bak now holds 40_000

    // The user breaks the file by hand, so that it is not TOML any more.
    fs::write(store.path(), "[network]\nlisten_port = 41000\nupnp = \n").unwrap();
    let l = store.load();
    assert!(matches!(l.source, LoadSource::Backup(_)), "{:?}", l.source);
    assert_eq!(l.value.network.listen_port, 40_000, "last known good");
    let err = l.error.expect("the reason is reported");
    assert!(matches!(err, DocError::Parse { line: Some(3), .. }), "{err}");
    assert!(fs::read_to_string(store.path()).unwrap().contains("upnp = \n"), "the broken file is left for the user to fix");

    // A newer format is never half-read either.
    fs::write(store.path(), "version = 99\n[network]\nlisten_port = 41000\n").unwrap();
    let l = store.load();
    assert!(matches!(l.source, LoadSource::Backup(_)), "{:?}", l.source);
    assert!(matches!(l.error, Some(DocError::Unsupported { .. })), "{:?}", l.error);

    // Saving over a broken file keeps it as .rejected and does not poison .bak.
    fs::write(store.path(), "garbage = [").unwrap();
    c.network.listen_port = 42_000;
    store.save(&c).unwrap();
    assert_eq!(fs::read_to_string(dir.path().join("config.toml.rejected")).unwrap(), "garbage = [");
    let bak = Config::parse_str(&fs::read_to_string(dir.path().join("config.toml.bak")).unwrap()).unwrap().value;
    assert_eq!(bak.network.listen_port, 40_000, ".bak still the last good one");
}

/// One bad value costs that value only: every other setting of the file stays in force (the
/// defaults would move the user's .torrent files to the Trash and switch agents off), the bad one
/// takes the last good value, from the backup the loader would fall back to, and is named with it;
/// the file is not written.
#[test]
fn a_bad_value_takes_the_last_good_value_from_the_backup_and_everything_else_is_kept() {
    use trs_state::config::{AfterAdd, Encryption};
    let dir = TempDir::new("salvage");
    let store = DocStore::<Config>::new(dir.path());
    let mut c = Config::default();
    c.network.listen_port = 40_000;
    c.network.encryption = Encryption::Require;
    c.folders.torrent_after_add = AfterAdd::Leave;
    store.save(&c).unwrap();
    c.network.listen_port = 40_001;
    store.save(&c).unwrap(); // .bak holds the first: the last good file before the hand edit
    let text = "[agents]\npermission = \"full\"\n[folders]\ntorrent_after_add = \"keep\"\n[network]\nlisten_port = 80\nencryption = \"always\"\nupnp = false\n";
    fs::write(store.path(), text).unwrap();
    let before = names(dir.path());

    let l = store.load();
    assert_eq!(l.source, LoadSource::Repaired);
    assert!(l.error.is_none(), "{:?}", l.error);
    assert_eq!(l.value.agents.permission, trs_state::config::Permission::Full);
    assert!(!l.value.network.upnp);
    assert_eq!(l.value.folders.torrent_after_add, AfterAdd::Leave);
    assert_eq!(l.value.network.listen_port, 40_000, "the last good file's, not the default");
    assert_eq!(l.value.network.encryption, Encryption::Require);
    let w: Vec<String> = l.warnings.iter().map(ToString::to_string).collect();
    assert_eq!(w.len(), 3, "{w:?}");
    let named = |key: &str, bad: &str, used: &str| {
        w.iter().any(|m| {
            m.starts_with(&format!("{key}: {bad} is not a valid value (")) && m.ends_with(&format!("; the last good value {used} from the backup is used until it is fixed"))
        })
    };
    assert!(named("folders.torrent_after_add", "\"keep\"", "\"leave\""), "{w:?}");
    assert!(named("network.listen_port", "80", "40000"), "{w:?}");
    assert!(named("network.encryption", "\"always\"", "\"require\""), "{w:?}");
    assert_eq!(fs::read_to_string(store.path()).unwrap(), text, "the user's file is not rewritten");
    assert_eq!(names(dir.path()), before, "nothing is written next to it either");

    // The strict read still refuses it (hot reload keeps the running value); the lenient one does not.
    assert!(store.read_strict().is_err());
    assert_eq!(store.read_lenient().unwrap().value, l.value);
}

/// With no backup, a bad value of a setting that moves, trashes or deletes the user's files takes
/// the choice that leaves them alone, not its default; any other takes its default.
#[test]
fn without_a_backup_a_file_setting_takes_the_choice_that_leaves_files_alone() {
    use trs_state::config::AfterAdd;
    let dir = TempDir::new("salvage-safe");
    let store = DocStore::<Config>::new(dir.path());
    fs::write(store.path(), "[folders]\ntorrent_after_add = \"keep\"\nby_status = \"yes\"\n[network]\nlisten_port = 80\n").unwrap();
    let l = store.load();
    assert_eq!(l.source, LoadSource::Repaired);
    assert_eq!(l.value.folders.torrent_after_add, AfterAdd::Leave, "not the default (Trash)");
    assert!(!l.value.folders.by_status, "nothing is filed into status folders");
    assert_eq!(l.value.network.listen_port, 0);
    let w: Vec<String> = l.warnings.iter().map(ToString::to_string).collect();
    assert_eq!(w.len(), 3, "{w:?}");
    assert!(w.iter().any(|m| m.starts_with("folders.torrent_after_add: \"keep\" is not a valid value (") && m.ends_with("; \"leave\", which leaves your files alone, is used until it is fixed")), "{w:?}");
    assert!(w.iter().any(|m| m.starts_with("folders.by_status: \"yes\" is not a valid value (") && m.ends_with("; false, which leaves your files alone, is used until it is fixed")), "{w:?}");
    assert!(w.iter().any(|m| m.starts_with("network.listen_port: 80 is not a valid value (") && m.ends_with("; the default 0 is used until it is fixed")), "{w:?}");
}

/// "archive" without a usable archive folder: the choice that cannot work is the one replaced, so
/// the file keeps every other value (it used to fall back as a whole, to the defaults with none:
/// the Trash for .torrent files and agents off).
#[test]
fn archive_without_a_folder_leaves_the_torrent_files_alone_and_keeps_the_rest() {
    use trs_state::config::{AfterAdd, Permission};
    for folder in ["", "archive_dir = \"relative/path\"\n"] {
        let dir = TempDir::new("salvage-archive");
        let store = DocStore::<Config>::new(dir.path());
        fs::write(store.path(), format!("[agents]\npermission = \"full\"\n[folders]\ntorrent_after_add = \"archive\"\n{folder}")).unwrap();
        let l = store.load();
        assert_eq!(l.source, LoadSource::Repaired, "{folder:?}: {:?}", l.error);
        assert_eq!(l.value.folders.torrent_after_add, AfterAdd::Leave, "{folder:?}");
        assert_eq!(l.value.folders.archive_dir, None);
        assert_eq!(l.value.agents.permission, Permission::Full, "the rest is kept");
        let w: Vec<String> = l.warnings.iter().map(ToString::to_string).collect();
        assert!(
            w.iter().any(|m| m.starts_with("folders.torrent_after_add: \"archive\" is not a valid value (\"archive\" needs folders.archive_dir")
                && m.ends_with("; \"leave\", which leaves your files alone, is used until it is fixed")),
            "{w:?}"
        );
        assert_eq!(w.len(), 1 + folder.len().min(1), "{w:?}");
    }
}

/// A backup without a value for the key (or none that passes the checks in the repaired file)
/// gives way to the next choice: the one that leaves files alone, then the default.
#[test]
fn a_backup_value_is_used_only_when_it_is_there_and_passes_the_checks() {
    use trs_state::config::AfterAdd;
    let dir = TempDir::new("salvage-partial");
    let store = DocStore::<Config>::new(dir.path());
    // A hand-written good file as the last good one: it has no torrent_after_add.
    fs::write(dir.path().join("config.toml.bak"), "[network]\nmax_connections = 100\nmax_connections_per_torrent = 80\n").unwrap();
    // 80 per torrent does not fit under this file's 60 in all: the default 50 does.
    fs::write(store.path(), "[folders]\ntorrent_after_add = \"keep\"\n[network]\nmax_connections = 60\nmax_connections_per_torrent = \"many\"\n").unwrap();
    let l = store.load();
    assert_eq!(l.source, LoadSource::Repaired);
    assert_eq!(l.value.folders.torrent_after_add, AfterAdd::Leave);
    assert_eq!((l.value.network.max_connections, l.value.network.max_connections_per_torrent), (60, 50));
    let w: Vec<String> = l.warnings.iter().map(ToString::to_string).collect();
    assert_eq!(w.len(), 2, "{w:?}");
    assert!(w.iter().any(|m| m.starts_with("folders.torrent_after_add: \"keep\"") && m.ends_with("which leaves your files alone, is used until it is fixed")), "{w:?}");
    assert!(
        w.iter().any(|m| m.starts_with("network.max_connections_per_torrent: \"many\" is not a valid value (") && m.ends_with("; the default 50 is used until it is fixed")),
        "{w:?}"
    );
}

#[test]
fn an_out_of_range_value_and_a_bad_list_entry_cost_only_themselves() {
    let dir = TempDir::new("salvage-checks");
    let store = DocStore::<Config>::new(dir.path());
    let (a, b) = (abs("/w/a"), abs("/w/b"));
    let text = format!(
        "[network]\nlisten_port = 80\ndht = false\n\n[[folders.watch]]\npath = \"{a}\"\nafter_add = \"keep\"\n\n[[folders.watch]]\npath = \"{b}\"\nafter_add = \"leave\"\n\n[[folders.watch]]\npath = \"{b}\"\n"
    );
    fs::write(store.path(), win(&text)).unwrap();
    let l = store.load();
    assert_eq!(l.source, LoadSource::Repaired);
    assert_eq!(l.value.network.listen_port, 0, "the default (automatic)");
    assert!(!l.value.network.dht);
    // The entry with a bad value goes, and so does the duplicate; the good one stays.
    assert_eq!(l.value.folders.watch.len(), 1, "{:?}", l.value.folders.watch);
    assert_eq!(l.value.folders.watch[0].path, b);
    let w: Vec<String> = l.warnings.iter().map(ToString::to_string).collect();
    assert_eq!(w.len(), 3, "{w:?}");
    assert!(w.iter().any(|m| m.starts_with("folders.watch[0]: this entry is not valid (")), "{w:?}");
    assert!(w.iter().any(|m| m.starts_with("network.listen_port: 80 is not a valid value (must be") && m.ends_with("the default 0 is used until it is fixed")), "{w:?}");
    assert!(w.iter().any(|m| m.starts_with("folders.watch[1]: this entry is not valid (duplicate")), "{w:?}");
}

#[test]
fn no_backup_and_broken_file_means_defaults_plus_error() {
    let dir = TempDir::new("nobak");
    fs::write(dir.path().join("rules.toml"), "[[rule]\n").unwrap();
    let l = DocStore::<RuleSet>::new(dir.path()).load();
    assert_eq!(l.source, LoadSource::Defaults);
    assert!(l.error.is_some());
    assert_eq!(l.value, RuleSet::default());
}

#[test]
fn oversized_and_non_utf8_files_are_rejected_cleanly() {
    let dir = TempDir::new("hostile");
    let store = DocStore::<Config>::new(dir.path());
    fs::write(store.path(), vec![b'#'; 2 << 20]).unwrap();
    assert!(matches!(store.read_strict(), Err(DocError::TooLarge { .. })));
    fs::write(store.path(), [0xff, 0xfe, 0x00, 0x80]).unwrap();
    let e = store.read_strict().unwrap_err();
    assert!(e.to_string().contains("UTF-8"), "{e}");
    assert!(store.load().error.is_some());
}

#[test]
fn unknown_keys_are_warned_about_and_preserved_across_saves() {
    let dir = TempDir::new("unknown");
    let store = DocStore::<Config>::new(dir.path());
    fs::write(store.path(), "[network]\nlisten_port = 40000\nfuture_option = \"x\"\n[from_the_future]\na = 1\n").unwrap();
    let l = store.load();
    assert_eq!(l.warnings.len(), 2);
    let mut c = l.value;
    c.network.listen_port = 40_001;
    store.save(&c).unwrap();
    let text = fs::read_to_string(store.path()).unwrap();
    assert!(text.contains("future_option = \"x\"") && text.contains("[from_the_future]"), "{text}");
}

/// A config.toml written by a developer build of the old private-torrent override still loads.
#[test]
fn a_retired_developer_key_loads_and_is_kept() {
    let dir = TempDir::new("retired");
    let store = DocStore::<Config>::new(dir.path());
    fs::write(store.path(), "[transfers]\ndeveloper_allow_private = true\n").unwrap();
    let l = store.load();
    assert!(l.error.is_none(), "{:?}", l.error);
    store.save(&l.value).unwrap();
    assert!(fs::read_to_string(store.path()).unwrap().contains("developer_allow_private"));
}

#[test]
fn rules_and_trackers_documents_roundtrip_through_the_store() {
    let dir = TempDir::new("others");
    let rs = DocStore::<RuleSet>::new(dir.path());
    let mut rules = rs.load().value;
    rules.add_rule(Rule { id: "tl".into(), name: "TL".into(), priority: Some(Priority::High), ..Rule::default() }, None).unwrap();
    rs.save(&rules).unwrap();
    assert_eq!(rs.load().value, rules);

    let ts = DocStore::<TrackerProfiles>::new(dir.path());
    let mut profiles = ts.load().value;
    profiles.add(Profile::suggest_from_announce("https://tracker.example.org/a/0123456789abcdef0123456789abcdef/announce?k=SECRETVALUE").unwrap()).unwrap();
    ts.save(&profiles).unwrap();
    let text = fs::read_to_string(ts.path()).unwrap();
    assert!(!text.contains("SECRETVALUE") && !text.contains("0123456789abcdef"), "passkey leaked into trackers.toml:\n{text}");
    assert_eq!(ts.load().value, profiles);
}

#[test]
fn saving_invalid_values_is_refused_before_touching_the_file() {
    let dir = TempDir::new("refuse");
    let store = DocStore::<Config>::new(dir.path());
    let mut c = store.load().value;
    let before = fs::read(store.path()).unwrap();
    c.network.listen_port = 5;
    let e = store.save(&c).unwrap_err();
    assert!(matches!(e, DocError::Invalid { .. }));
    assert_eq!(fs::read(store.path()).unwrap(), before);
}

// ------------------------------------------------------------------ hot reload

fn bump(store: &DocStore<Config>, f: impl FnOnce(&mut Config)) {
    let mut c = store.read_strict().unwrap().value;
    f(&mut c);
    store.save(&c).unwrap();
}

#[test]
fn poller_delivers_valid_changes_and_withholds_invalid_ones() {
    let dir = TempDir::new("poll");
    let store = DocStore::<Config>::new(dir.path());
    let running = store.load().value;
    let mut p = Poller::new(store.clone(), running.clone());
    assert!(p.poll().is_none(), "nothing changed");

    bump(&store, |c| c.network.listen_port = 50_001);
    match p.poll() {
        Some(PollEvent::Reloaded(parsed)) => assert_eq!(parsed.value.network.listen_port, 50_001),
        other => panic!("{other:?}"),
    }
    assert_eq!(p.running().network.listen_port, 50_001);
    assert!(p.poll().is_none(), "one event per change");

    // Saving identical content, or touching the file without changing meaning, is silent.
    let text = fs::read_to_string(store.path()).unwrap();
    fs::write(store.path(), format!("{text}\n# a comment\n")).unwrap();
    assert!(p.poll().is_none());

    // An invalid edit is reported only once it persists for a second poll, and never replaces the running value.
    fs::write(store.path(), "[network]\nlisten_port = 80\n").unwrap();
    assert!(p.poll().is_none(), "first sighting: maybe the editor is mid-write");
    match p.poll() {
        Some(PollEvent::Rejected(DocError::Invalid { errors, .. })) => assert_eq!(errors[0].path, "network.listen_port"),
        other => panic!("{other:?}"),
    }
    assert_eq!(p.running().network.listen_port, 50_001, "running config untouched");
    assert!(p.poll().is_none(), "reported once");

    // Fixing it delivers the new value.
    fs::write(store.path(), "[network]\nlisten_port = 50002\n").unwrap();
    assert!(matches!(p.poll(), Some(PollEvent::Reloaded(_))));
    assert_eq!(p.running().network.listen_port, 50_002);
}

#[test]
fn a_transient_invalid_state_never_surfaces() {
    let dir = TempDir::new("midwrite");
    let store = DocStore::<Config>::new(dir.path());
    let mut p = Poller::new(store.clone(), store.load().value);
    fs::write(store.path(), "[network\nlisten_po").unwrap(); // half-written
    assert!(p.poll().is_none());
    fs::write(store.path(), "[network]\nlisten_port = 50003\n").unwrap(); // finished
    assert!(matches!(p.poll(), Some(PollEvent::Reloaded(_))));
}

#[test]
fn removed_file_is_reported_and_running_config_kept() {
    let dir = TempDir::new("removed");
    let store = DocStore::<Config>::new(dir.path());
    let mut p = Poller::new(store.clone(), store.load().value);
    fs::remove_file(store.path()).unwrap();
    assert!(matches!(p.poll(), Some(PollEvent::Rejected(DocError::Io { .. }))));
    assert_eq!(p.running(), &Config::default());
    assert!(p.poll().is_none());
}

#[test]
fn own_saves_are_acknowledged_without_an_event() {
    let dir = TempDir::new("ack");
    let store = DocStore::<Config>::new(dir.path());
    let mut c = store.load().value;
    let mut p = Poller::new(store.clone(), c.clone());
    c.network.dht = false;
    store.save(&c).unwrap();
    p.set_running(c);
    assert!(p.poll().is_none());
}

#[test]
fn background_watcher_delivers_events_and_stops_on_drop() {
    let dir = TempDir::new("watch");
    let store = DocStore::<Config>::new(dir.path());
    let p = Poller::new(store.clone(), store.load().value);
    let (tx, rx) = mpsc::channel();
    let handle = spawn_watcher(p, Duration::from_millis(30), move |ev| {
        let _ = tx.send(ev);
    })
    .unwrap();
    bump(&store, |c| c.network.listen_port = 50_010);
    match rx.recv_timeout(Duration::from_secs(5)).expect("event") {
        PollEvent::Reloaded(p) => assert_eq!(p.value.network.listen_port, 50_010),
        other => panic!("{other:?}"),
    }
    handle.stop();
    bump(&store, |c| c.network.listen_port = 50_011);
    assert!(rx.recv_timeout(Duration::from_millis(200)).is_err(), "no events after stop");
}

#[test]
fn watchers_share_one_thread_stop_independently_and_the_thread_restarts() {
    let a_dir = TempDir::new("watch-a");
    let b_dir = TempDir::new("watch-b");
    let a_store = DocStore::<Config>::new(a_dir.path());
    let b_store = DocStore::<Config>::new(b_dir.path());
    let start = |store: &DocStore<Config>| {
        let (tx, rx) = mpsc::channel();
        let p = Poller::new(store.clone(), store.load().value);
        let h = spawn_watcher(p, Duration::from_millis(20), move |ev| {
            let _ = tx.send(ev);
        })
        .unwrap();
        (h, rx)
    };
    let (a, a_rx) = start(&a_store);
    let (b, b_rx) = start(&b_store);
    bump(&a_store, |c| c.network.listen_port = 50_020);
    bump(&b_store, |c| c.network.listen_port = 50_021);
    match a_rx.recv_timeout(Duration::from_secs(5)).expect("a event") {
        PollEvent::Reloaded(p) => assert_eq!(p.value.network.listen_port, 50_020),
        other => panic!("{other:?}"),
    }
    match b_rx.recv_timeout(Duration::from_secs(5)).expect("b event") {
        PollEvent::Reloaded(p) => assert_eq!(p.value.network.listen_port, 50_021),
        other => panic!("{other:?}"),
    }
    // Stopping one leaves the other running.
    a.stop();
    bump(&a_store, |c| c.network.listen_port = 50_022);
    bump(&b_store, |c| c.network.listen_port = 50_023);
    match b_rx.recv_timeout(Duration::from_secs(5)).expect("b event after a stopped") {
        PollEvent::Reloaded(p) => assert_eq!(p.value.network.listen_port, 50_023),
        other => panic!("{other:?}"),
    }
    assert!(a_rx.recv_timeout(Duration::from_millis(150)).is_err(), "a is silent after stop");
    // With the last one gone the thread ends; a new watcher starts it again.
    drop(b);
    std::thread::sleep(Duration::from_millis(100));
    let (c, c_rx) = start(&a_store);
    bump(&a_store, |c| c.network.listen_port = 50_024);
    match c_rx.recv_timeout(Duration::from_secs(5)).expect("event after restart") {
        PollEvent::Reloaded(p) => assert_eq!(p.value.network.listen_port, 50_024),
        other => panic!("{other:?}"),
    }
    drop(c);
}

#[test]
fn a_version_1_config_is_migrated_on_load_with_a_backup() {
    let dir = TempDir::new("migrate-v1");
    let path = dir.path().join("config.toml");
    let v1 = win("version = 1\n[network]\nlisten_port = 51413\n[folders]\ndownload_dir = \"/Volumes/Media/Done\"\nincomplete_dir = \"/Volumes/Media/Inc\"\n");
    fs::write(&path, &v1).unwrap();
    let store = DocStore::<Config>::new(dir.path());
    let l = store.load();
    assert_eq!(l.source, LoadSource::File);
    assert!(l.error.is_none(), "{:?}", l.error);
    assert_eq!(l.value.version, trs_state::config::CONFIG_VERSION);
    assert_eq!(l.value.network.listen_port, 51413);
    assert_eq!(l.value.folders.download_dir, abs("/Volumes/Media/Done"));
    assert_eq!(l.value.transfers.large_torrent_gb, 50);
    assert!(l.value.folders.categories.is_empty());
    // The file on disk is now version 3 and the old text is in the backups.
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains(&format!("version = {}", trs_state::config::CONFIG_VERSION)), "{text}");
    assert!(text.contains("listen_port = 51413"));
    assert_eq!(fs::read_to_string(dir.path().join("config.toml.bak")).unwrap(), v1);
    let backups = store.backups();
    assert_eq!(backups.len(), 1);
    assert_eq!(fs::read_to_string(&backups[0]).unwrap(), v1);
    // Loading again changes nothing.
    let again = store.load();
    assert_eq!(again.value, l.value);
    assert_eq!(store.backups().len(), 1);
}

#[test]
fn a_config_with_the_retired_seeding_folder_loads_and_is_rewritten_without_it() {
    let dir = TempDir::new("migrate-seeding");
    let path = dir.path().join("config.toml");
    let old = win("version = 2\n[folders]\ndownload_dir = \"~/Downloads\"\nplacement = \"hardlink\"\n[folders.templates]\nmovie = \"~/Media/{title}\"\n[folders.seeding]\nmode = \"fixed\"\ndir = \"/Volumes/Media/Seed\"\n");
    fs::write(&path, &old).unwrap();
    let store = DocStore::<Config>::new(dir.path());
    let l = store.load();
    assert!(l.error.is_none(), "{:?}", l.error);
    assert!(l.warnings.is_empty(), "{:?}", l.warnings);
    // The seeding folder became the finished location; the key is gone from the file, the old text is kept.
    assert_eq!(l.value.folders.download_dir, abs("/Volumes/Media/Seed"));
    let text = fs::read_to_string(&path).unwrap();
    assert!(!text.contains("seeding") && !text.contains("template") && !text.contains("placement") && text.contains(&abs("/Volumes/Media/Seed")), "{text}");
    assert_eq!(fs::read_to_string(dir.path().join("config.toml.bak")).unwrap(), old);
    assert_eq!(store.load().value, l.value);
    assert_eq!(store.backups().len(), 1);
}

#[test]
fn a_version_1_state_database_is_upgraded_with_a_backup() {
    use trs_state::Store;
    let dir = TempDir::new("migrate-db");
    // Build a database at schema 1 with the old schema description, then open it with this build.
    static V1: trs_state::store::Schema = trs_state::store::Schema { current: 1, migrations: &[] };
    {
        let opts = trs_state::OpenOptions { schema: &V1, ..trs_state::OpenOptions::default() };
        let opened = Store::open_with(dir.path(), &opts).unwrap();
        let mut rec = trs_state::TorrentRecord::new("seed", 10, 1);
        rec.save_root = 0;
        opened.store.put_torrent([7; 20], &rec).unwrap();
        opened.store.close().unwrap();
    }
    let opened = Store::open(dir.path()).unwrap();
    let m = opened.report.migrated.expect("migrated");
    assert_eq!((m.from, m.to), (1, 2));
    assert!(m.backup.exists());
    let rec = opened.store.get_torrent([7; 20]).unwrap().unwrap();
    assert_eq!(rec.name, "seed");
    assert_eq!(rec.stage_root, None);
}

#[test]
fn watch_baselines_are_stamped_once_and_restamped_when_re_enabled() {
    let mut c = Config::default();
    assert_eq!(c.folders.watch[0].since, None);
    assert!(c.stamp_watch_baselines(None, 1000));
    assert_eq!(c.folders.watch[0].since, Some(1000));
    // Stamping again later keeps the baseline.
    assert!(!c.stamp_watch_baselines(None, 2000));
    assert_eq!(c.folders.watch[0].since, Some(1000));
    // A new entry gets "now"; the old one keeps its baseline.
    let prev = c.clone();
    c.folders.watch.push(trs_state::config::WatchFolder { path: "~/Torrents".into(), ..Default::default() });
    assert!(c.stamp_watch_baselines(Some(&prev), 3000));
    assert_eq!((c.folders.watch[0].since, c.folders.watch[1].since), (Some(1000), Some(3000)));
    // An entry that was disabled and is switched on again starts a new baseline.
    let mut prev = c.clone();
    prev.folders.watch[1].enabled = false;
    assert!(c.stamp_watch_baselines(Some(&prev), 4000));
    assert_eq!((c.folders.watch[0].since, c.folders.watch[1].since), (Some(1000), Some(4000)));
    // A baseline copied along with an edited path is not trusted.
    let prev = c.clone();
    c.folders.watch[1].path = "~/Elsewhere".into();
    assert!(c.stamp_watch_baselines(Some(&prev), 5000));
    assert_eq!(c.folders.watch[1].since, Some(5000));
    // And it survives a save/load round trip.
    let text = c.render().unwrap();
    assert!(text.contains("since = 1000"), "{text}");
    assert_eq!(Config::parse_str(&text).unwrap().value, c);
}

#[test]
fn category_locations_round_trip_and_validate() {
    let text = &win("[folders.categories.tv]\nincomplete_dir = \"/Volumes/Fast/inc\"\nfinished_dir = \"/Volumes/Media/TV\"\n\
                [folders.categories.movie]\nfinished_dir = \"/Volumes/Media/Movies\"\n");
    let p = Config::parse_str(text).unwrap();
    let c = &p.value.folders.categories;
    assert_eq!(c.tv.incomplete_dir.as_deref(), Some(abs("/Volumes/Fast/inc").as_str()));
    assert_eq!(c.movie.finished_dir.as_deref(), Some(abs("/Volumes/Media/Movies").as_str()));
    assert!(c.music.is_empty());
    let out = p.value.render().unwrap();
    assert!(out.contains("[folders.categories.tv]") && !out.contains("[folders.categories.music]"), "{out}");
    assert_eq!(Config::parse_str(&out).unwrap().value, p.value);
    let Err(DocError::Invalid { errors, .. }) =
        Config::parse_str("[folders.categories.tv]\nfinished_dir = \"relative/tv\"\nincomplete_dir = \"relative\"\n")
    else {
        panic!("expected validation errors")
    };
    let paths: Vec<_> = errors.iter().map(|e| e.path.as_str()).collect();
    assert!(paths.contains(&"folders.categories.tv.finished_dir"), "{paths:?}");
    assert!(paths.contains(&"folders.categories.tv.incomplete_dir"), "{paths:?}");
}

#[test]
fn the_start_delay_defaults_to_ten_seconds_round_trips_and_is_validated() {
    // Default, also for a file written before the setting existed.
    assert_eq!(Config::default().transfers.start_delay_secs, 10);
    assert_eq!(Config::parse_str("[transfers]\nmax_active_downloads = 3\n").unwrap().value.transfers.start_delay_secs, 10);
    // 0 (off) and a custom value survive a save and load.
    let dir = TempDir::new("start-delay");
    let store = DocStore::<Config>::new(dir.path());
    for secs in [0u32, 25, 600] {
        let mut c = Config::default();
        c.transfers.start_delay_secs = secs;
        store.save(&c).unwrap();
        assert!(fs::read_to_string(dir.path().join("config.toml")).unwrap().contains(&format!("start_delay_secs = {secs}")));
        let l = store.load();
        assert!(l.error.is_none(), "{:?}", l.error);
        assert_eq!(l.value.transfers.start_delay_secs, secs);
    }
    // Out of range is refused with the field's path.
    let Err(DocError::Invalid { errors, .. }) = Config::parse_str("[transfers]\nstart_delay_secs = 601\n") else {
        panic!("expected a validation error")
    };
    assert!(errors.iter().any(|e| e.path == "transfers.start_delay_secs"), "{errors:?}");
}

#[test]
fn private_min_seed_defaults_to_ten_days_validates_and_a_v3_file_migrates_keeping_everything_else() {
    assert_eq!(Config::default().transfers.private_min_seed_secs, 10 * 86_400);
    // A version 3 file without the key: loaded with the default, rewritten as version 4 (with the
    // usual backup of the old file), the user's other settings and unknown keys kept.
    let dir = TempDir::new("private-floor");
    let store = DocStore::<Config>::new(dir.path());
    let v3 = "version = 3\n[network]\nlisten_port = 40123\nfuture_option = \"x\"\n[transfers]\nmax_active_downloads = 5\nupload_limit = 123456\n[from_the_future]\na = 1\n";
    fs::write(store.path(), v3).unwrap();
    let l = store.load();
    assert!(l.error.is_none(), "{:?}", l.error);
    let t = &l.value.transfers;
    assert_eq!((t.private_min_seed_secs, t.max_active_downloads, t.upload_limit), (864_000, 5, 123_456));
    assert_eq!(l.value.network.listen_port, 40_123);
    let text = fs::read_to_string(store.path()).unwrap();
    assert!(text.contains(&format!("version = {}", trs_state::config::CONFIG_VERSION)) && text.contains("private_min_seed_secs = 864000"), "{text}");
    assert!(text.contains("future_option = \"x\"") && text.contains("[from_the_future]"), "{text}");
    assert!(text.contains("listen_port = 40123") && text.contains("upload_limit = 123456"), "{text}");
    assert!(!store.backups().is_empty() || dir.path().join("config.toml.bak").exists(), "the v3 file is kept as a backup");
    // Off and custom values survive a save and load; out of range is refused with the field path.
    for secs in [0u64, 3 * 86_400, 30 * 86_400, 12_345] {
        let mut c = l.value.clone();
        c.transfers.private_min_seed_secs = secs;
        store.save(&c).unwrap();
        assert_eq!(store.load().value.transfers.private_min_seed_secs, secs);
    }
    let Err(DocError::Invalid { errors, .. }) = Config::parse_str("[transfers]\nprivate_min_seed_secs = 99999999999\n") else {
        panic!("expected a validation error")
    };
    assert!(errors.iter().any(|e| e.path == "transfers.private_min_seed_secs"), "{errors:?}");
}
