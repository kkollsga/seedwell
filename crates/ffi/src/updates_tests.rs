use super::*;
use crate::CoreOptions;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;

const T0: u64 = 1_800_000_000_000;
const MIN: u64 = 60_000;
const HOUR: u64 = 3_600_000;

fn mock() -> (Arc<Core>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let core = Core::new(CoreOptions { config_dir: dir.path().display().to_string(), mock: true, allow_loopback_trackers: false, forbidden_root: None }).unwrap();
    (core, dir)
}

fn current() -> String {
    env!("CARGO_PKG_VERSION").to_owned()
}

/// The next patch version after the running one.
fn newer() -> String {
    let (a, b, c) = parse_version(&current()).unwrap();
    format!("{a}.{b}.{}", c + 1)
}

/// A release's latest.json with the Mac's fields on top and a Windows entry, for `v`.
fn latest(v: &str) -> String {
    let sha = "a".repeat(64);
    format!(
        r#"{{"version":"{v}","tag":"v{v}","notes_url":"https://github.com/kkollsga/seedwell/releases/tag/v{v}","minimum_macos":"15.0","zip":"Seedwell-{v}.zip","sha256":"{sha}","windows":{{"zip":"Seedwell-{v}-windows-x64.zip","sha256":"{sha}","minimum_windows":"10.0.19041"}}}}"#
    )
}

/// A loopback server answering every request with `status` and `body`; returns its base URL (a repo).
fn serve(status: u16, body: String) -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    std::thread::spawn(move || {
        for s in l.incoming() {
            let Ok(mut s) = s else { return };
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            let _ = write!(s, "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        }
    });
    base
}

fn updates(core: &Arc<Core>, release: bool, repo: &str, now: u64) -> Arc<Updates> {
    Updates::new(core.clone(), release, Some(repo.to_owned()), os_version().into(), None, "/Applications".into(), String::new(), vec!["--background".into()], now)
}

/// A development build `<current>.<n>` of this updater, with `installer` as the app's own installer.
fn dev_updates(core: &Arc<Core>, n: u32, repo: &str, installer: &Path, now: u64) -> Arc<Updates> {
    Updates::new(
        core.clone(),
        true,
        Some(repo.to_owned()),
        os_version().into(),
        Some(format!("{}.{n}", current())),
        "/Applications".into(),
        installer.display().to_string(),
        vec!["--background".into()],
        now,
    )
}

/// A system version new enough for the test releases on either system.
fn os_version() -> &'static str {
    if os::RELEASE_SECTION.is_some() { "10.0.22631" } else { "26.0" }
}

#[test]
fn versions_are_plain_and_system_versions_take_one_to_three_parts() {
    assert_eq!(parse_version("0.1.9"), Some((0, 1, 9)));
    for bad in ["v0.1.9", "0.1", "0.1.9.1", "0.1.9-beta", "", "1..2", "0.1.x"] {
        assert_eq!(parse_version(bad), None, "{bad}");
    }
    assert_eq!(parse_system_version("15"), Some((15, 0, 0)));
    assert_eq!(parse_system_version("15.4"), Some((15, 4, 0)));
    assert_eq!(parse_system_version("10.0.19045.4529"), Some((10, 0, 19045)));
    assert_eq!(parse_system_version("x"), None);
}

#[test]
fn latest_json_gives_each_system_its_own_entry_and_refuses_anything_else() {
    let mac = |d: &str| parse_latest(d.as_bytes(), None, "minimum_macos", |v| format!("Seedwell-{v}.zip"));
    let win = |d: &str| parse_latest(d.as_bytes(), Some("windows"), "minimum_windows", |v| format!("Seedwell-{v}-windows-x64.zip"));
    let doc = latest("0.2.0");
    assert_eq!(mac(&doc).unwrap().minimum_system, Some((15, 0, 0)));
    assert_eq!(win(&doc).unwrap().minimum_system, Some((10, 0, 19041)));
    assert_eq!(mac(&doc).unwrap().notes_url.as_deref(), Some("https://github.com/kkollsga/seedwell/releases/tag/v0.2.0"));
    // A release without a Windows build offers nothing on Windows; the Mac's fields alone still work for the Mac.
    let mac_only = r#"{"version":"0.2.0","tag":"v0.2.0","zip":"Seedwell-0.2.0.zip","sha256":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}"#;
    assert!(mac(mac_only).is_some() && win(mac_only).is_none());
    for bad in [
        doc.replace("\"tag\":\"v0.2.0\"", "\"tag\":\"v0.2.1\""),
        doc.replace("Seedwell-0.2.0.zip", "Other.zip"),
        doc.replacen(&"a".repeat(64), "abc", 1),
        doc.replace("https://github.com", "file://github.com"),
        doc.replace("\"15.0\"", "\"fifteen\""),
        "not json".into(),
        format!("{{\"pad\":\"{}\"}}", "x".repeat(70_000)),
    ] {
        assert!(mac(&bad).is_none(), "{}", &bad[..bad.len().min(120)]);
    }
}

#[test]
fn a_release_is_offered_when_it_is_newer_and_this_system_is_new_enough() {
    let l = |v: &str, min| Latest { version: v.into(), sha256: String::new(), notes_url: None, minimum_system: min };
    assert!(l("0.2.0", Some((15, 0, 0))).offered("0.1.9", "15.4"));
    assert!(!l("0.1.9", None).offered("0.1.9", "15.4"), "the same version");
    assert!(!l("0.1.8", None).offered("0.1.9", "15.4"), "an older one");
    assert!(!l("0.2.0", Some((16, 0, 0))).offered("0.1.9", "15.4"), "the system is too old");
    assert!(!l("0.2.0", Some((15, 0, 0))).offered("0.1.9", "?"), "an unreadable system version");
    assert!(l("0.10.0", None).offered("0.9.9", "15"), "numbers, not text");
}

#[test]
fn a_check_finds_a_newer_release_and_install_starts_its_installer() {
    let (core, dir) = mock();
    let repo = serve(200, latest(&newer()));
    let u = updates(&core, true, &repo, T0);
    assert_eq!(u.model(T0).status, format!("Seedwell {} · not checked yet", current()));
    u.check(false, T0 + MIN);
    let m = u.model(T0 + MIN);
    assert_eq!(m.status, format!("Seedwell {} is available", newer()));
    assert_eq!(m.install.as_ref().map(|b| b.title.clone()), Some(format!("Update to {}", newer())));
    assert_eq!(m.available_version, Some(newer()));
    assert!(m.notes_url.is_some());
    let c = u.install("/Applications".into(), T0 + 2 * MIN).unwrap();
    let all = c.argv.join(" ") + &c.env.iter().map(|e| e.value.clone()).collect::<String>();
    assert!(all.contains(&format!("{repo}/releases/download/v{}/{}", newer(), os::INSTALLER)), "{all}");
    assert!(all.contains("/Applications"));
    assert_eq!(c.log, dir.path().join("logs").join("update.log").display().to_string());
    assert_eq!(u.model(T0 + 2 * MIN).status, format!("Installing {}…", newer()));
    assert!(!u.model(T0 + 2 * MIN).check_now.enabled, "nothing else while it installs");
    assert!(u.install("/Applications".into(), T0 + 2 * MIN).is_none(), "one install at a time");
    // Still running 90 s later: it did not work.
    let m = u.model(T0 + 2 * MIN + 90_000);
    assert_eq!(m.status, format!("Update to {} failed", newer()));
    assert!(m.show_log.is_some());
}

#[test]
fn the_next_launch_says_whether_the_install_worked() {
    let (core, _d) = mock();
    let u = updates(&core, true, "http://127.0.0.1:9", T0);
    // The version this build is: the install worked.
    u.import_earlier(Some(T0), None, None, Some(current()));
    assert_eq!(u.model(T0).status, format!("Updated to {}", current()));
    let again = updates(&core, true, "http://127.0.0.1:9", T0 + HOUR);
    assert_ne!(again.model(T0 + HOUR).status, format!("Updated to {}", current()), "said once");
    // Another version: it did not.
    let (core, _d) = mock();
    let u = updates(&core, true, "http://127.0.0.1:9", T0);
    u.import_earlier(None, None, None, Some(newer()));
    assert_eq!(u.model(T0).status, format!("Update to {} failed", newer()));
    // What the earlier app kept is taken over once.
    u.import_earlier(Some(T0), Some(newer()), None, None);
    assert_eq!(u.model(T0).status, format!("Update to {} failed", newer()));
}

#[test]
fn checks_come_a_minute_after_launch_then_once_a_day_and_only_in_release_builds() {
    let (core, _d) = mock();
    let repo = serve(200, latest(&current()));
    assert_eq!(updates(&core, false, &repo, T0).next_due_ms(T0), None, "a development build checks when asked");
    let u = updates(&core, true, &repo, T0);
    assert_eq!(u.next_due_ms(T0), Some(MIN));
    assert!(!u.due(T0 + MIN - 1) && u.due(T0 + MIN));
    u.check(true, T0 + MIN);
    assert_eq!(u.model(T0 + MIN + 5 * MIN).status, format!("Seedwell {} is up to date · checked 5 minutes ago", current()));
    assert_eq!(u.next_due_ms(T0 + MIN), Some(24 * HOUR));
    // A clock that went back does not wait a day.
    let u = updates(&core, true, &repo, T0 - 10 * HOUR);
    assert_eq!(u.next_due_ms(T0 - 10 * HOUR), Some(MIN));
    // Checking switched off: no automatic check.
    let mut s = core.settings_load();
    s.updates.check = false;
    core.settings_save(s).unwrap();
    assert_eq!(updates(&core, true, &repo, T0).next_due_ms(T0), None);
}

#[test]
fn a_version_on_offer_is_checked_again_after_a_launch_it_may_have_been_withdrawn() {
    let (core, _d) = mock();
    let repo = serve(200, latest(&current()));
    let u = updates(&core, true, &repo, T0);
    u.import_earlier(Some(T0 - HOUR), Some(newer()), None, None);
    assert_eq!(u.next_due_ms(T0), Some(MIN), "not a day after the last check");
    u.check(true, T0 + MIN);
    assert_eq!(u.available_version(), None, "withdrawn");
}

#[test]
fn a_failed_check_says_why_unless_it_was_an_automatic_one() {
    let (core, _d) = mock();
    let repo = serve(500, String::new());
    let u = updates(&core, true, &repo, T0);
    u.check(true, T0);
    assert_eq!(u.model(T0).status, format!("Seedwell {} · not checked yet", current()), "silent: what it knew");
    u.check(false, T0);
    assert_eq!(u.model(T0).status, "Couldn't check: server answered 500");
    let (core, _d) = mock();
    let u = updates(&core, true, &serve(404, String::new()), T0);
    u.check(false, T0);
    assert_eq!(u.model(T0).status, "No releases yet");
    let (core, _d) = mock();
    let u = updates(&core, true, &serve(200, "{}".into()), T0);
    u.check(false, T0);
    assert_eq!(u.model(T0).status, "Couldn't check: unreadable release information");
}

#[test]
fn a_failing_automatic_check_is_retried_later_and_later_never_at_once() {
    let (core, _d) = mock();
    let repo = serve(500, String::new());
    let u = updates(&core, true, &repo, T0);
    assert_eq!(u.next_due_ms(T0), Some(MIN));
    u.check(true, T0 + MIN);
    assert_eq!(u.next_due_ms(T0 + MIN), Some(HOUR), "not at once (the old loop: due again immediately, forever)");
    u.check(true, T0 + MIN + HOUR);
    assert_eq!(u.next_due_ms(T0 + MIN + HOUR), Some(2 * HOUR));
    // A relaunch remembers the failures: no check a minute after launch, the backoff still holds.
    let at = T0 + MIN + HOUR + 10 * MIN;
    let again = updates(&core, true, &repo, at);
    assert_eq!(again.next_due_ms(at), Some(2 * HOUR - 10 * MIN));
    // Up to a day at most, however many failures.
    for i in 0..10 {
        again.check(true, at + i);
    }
    assert_eq!(again.next_due_ms(at + 9), Some(24 * HOUR));
    // A check that works resets it: the next is a day later, as usual.
    let ok = serve(200, latest(&current()));
    let fixed = updates(&core, true, &ok, at + 24 * HOUR);
    fixed.check(true, at + 24 * HOUR);
    assert_eq!(fixed.next_due_ms(at + 24 * HOUR), Some(24 * HOUR));
}

#[test]
fn a_release_without_a_build_for_this_system_is_nothing_on_offer_not_a_failure() {
    let mac_only = r#"{"version":"9.9.9","tag":"v9.9.9","zip":"Seedwell-9.9.9.zip","sha256":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}"#;
    assert!(release_without_this_system(mac_only.as_bytes(), Some("windows")));
    assert!(!release_without_this_system(mac_only.as_bytes(), None));
    assert!(!release_without_this_system(b"{}", Some("windows")));
    assert!(!release_without_this_system(latest("9.9.9").as_bytes(), Some("windows")));
    if os::RELEASE_SECTION.is_some() {
        let (core, _d) = mock();
        let u = updates(&core, true, &serve(200, mac_only.into()), T0);
        u.check(true, T0 + MIN);
        assert_eq!(u.available_version(), None);
        assert!(u.model(T0 + MIN).status.contains("is up to date"), "{}", u.model(T0 + MIN).status);
        assert_eq!(u.next_due_ms(T0 + MIN), Some(24 * HOUR));
    }
}

#[test]
fn an_isolated_run_never_checks_the_real_update_server() {
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let core = Core::new(CoreOptions {
        config_dir: dir.path().display().to_string(),
        mock: true,
        allow_loopback_trackers: false,
        forbidden_root: Some(home.path().display().to_string()),
    })
    .unwrap();
    let u = Updates::new(core, true, None, os_version().into(), None, String::new(), String::new(), vec![], T0);
    assert_eq!(u.next_due_ms(T0), None, "no automatic check");
    u.check(false, T0);
    assert_eq!(u.model(T0).status, "Couldn't check: a test run reaches no update server");
}

#[test]
fn it_installs_by_itself_only_with_both_switches_on_and_nothing_downloading() {
    let (core, _d) = mock();
    let u = updates(&core, true, &serve(200, latest(&newer())), T0);
    u.check(true, T0);
    assert!(!u.should_install(), "the install switch is off by default");
    let mut s = core.settings_load();
    s.updates.auto_install = true;
    core.settings_save(s).unwrap();
    assert!(!core.snapshot().downloading.is_empty());
    assert!(!u.should_install(), "the mock is downloading");
    let dev = updates(&core, false, &serve(200, latest(&newer())), T0);
    dev.check(false, T0);
    assert!(!dev.should_install(), "never in a development build");
}

#[test]
fn a_version_remembered_from_an_earlier_run_is_not_installed_before_a_check_confirms_it() {
    let (core, _d) = mock();
    let u = updates(&core, true, &serve(200, latest(&newer())), T0);
    u.import_earlier(Some(T0 - HOUR), Some(newer()), None, None);
    assert_eq!(u.available_version(), Some(newer()), "on offer, from the earlier run");
    assert!(!u.confirmed_offer(), "not before this run's check");
    u.check(true, T0 + MIN);
    assert!(u.confirmed_offer(), "confirmed by this run's check");
    // A check that finds it withdrawn ends the offer.
    let (core, _d) = mock();
    let u = updates(&core, true, &serve(200, latest(&current())), T0);
    u.import_earlier(Some(T0 - HOUR), Some(newer()), None, None);
    u.check(true, T0 + MIN);
    assert!(!u.confirmed_offer() && u.available_version().is_none());
}

#[test]
fn the_texts_of_a_development_build() {
    let (core, _d) = mock();
    let m = updates(&core, false, "http://127.0.0.1:9", T0).model(T0);
    assert_eq!((m.check_hint.as_str(), m.switches_enabled), ("Local build: checks only when you ask", false));
    assert_eq!((m.check_now.title.as_str(), m.check_now.enabled), ("Check Now", true));
    assert!(m.install.is_none() && m.show_log.is_none() && m.notes_url.is_none());
}

// MARK: Development builds

fn set_dev(core: &Arc<Core>, on: bool, folder: Option<&Path>) {
    let mut s = core.settings_load();
    s.updates.dev_builds = on;
    s.updates.dev_folder = folder.map(|f| f.display().to_string()).unwrap_or_default();
    core.settings_save(s).unwrap();
}

/// Nothing downloading or moving in the mock (an install waits for both): every torrent removed.
#[cfg(unix)]
fn idle(core: &Arc<Core>) {
    for _ in 0..100 {
        let s = core.snapshot();
        let q = crate::RowQuery { group: None, tracker_id: None, text: String::new(), category: None, offset: 0, limit: 10_000 };
        let mut ids: Vec<String> = s.downloading.iter().chain(&s.hidden_downloading).map(|r| r.id.clone()).collect();
        ids.extend(core.sharing_rows(q).into_iter().map(|r| r.id));
        if ids.is_empty() && s.moving_percent.is_none() {
            return;
        }
        core.remove(ids, false);
    }
    panic!("the mock still downloads or moves");
}

/// A stand-in for the app's installer: it writes its arguments and the state folder it was given to `ran`.
#[cfg(unix)]
fn fake_installer(dir: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let script = dir.join("install.sh");
    let ran = dir.join("ran");
    // Written aside and renamed: a reader never sees half of it.
    let r = ran.display();
    std::fs::write(&script, format!("printf '%s\\n' \"$@\" \"$SEEDWELL_INSTALL_STATE_DIR\" > '{r}.part' && mv '{r}.part' '{r}'\n")).unwrap();
    (script, ran)
}

#[cfg(unix)]
fn wait_for(p: &Path) -> String {
    for _ in 0..300 {
        if let Ok(t) = std::fs::read_to_string(p)
            && !t.is_empty()
        {
            return t;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!("{} never written", p.display());
}

#[test]
fn a_development_build_sorts_after_its_release_and_before_the_next() {
    assert_eq!(version_order("0.1.10"), Some((0, 1, 10, 0)));
    assert_eq!(version_order("0.1.10.3"), Some((0, 1, 10, 3)));
    assert!(version_order("0.1.10") < version_order("0.1.10.1"));
    assert!(version_order("0.1.10.9") < version_order("0.1.10.10"), "numbers, not text");
    assert!(version_order("0.1.10.99") < version_order("0.1.11"));
    for bad in ["0.1.10.0", "0.1.10.", "0.1.10.x", "0.1.10.3.1", "v0.1.10.3", "0.1"] {
        assert_eq!(version_order(bad), None, "{bad}");
    }
    assert!(is_development("0.1.10.3") && !is_development("0.1.10"));
    assert_eq!(shown("0.1.10.3"), "0.1.10.3 (development build)");
    assert_eq!(shown("0.1.10"), "0.1.10");
    assert_eq!(zip_version(&os::release_zip("0.1.10.3")).as_deref(), Some("0.1.10.3"));
    assert_eq!(zip_version(&os::release_zip("0.2.0")).as_deref(), Some("0.2.0"));
    for bad in ["Seedwell-0.1.10.3.tar.gz", "Other-0.1.10.zip", "Seedwell-.zip", "Seedwell-0.1.zip"] {
        assert_eq!(zip_version(bad), None, "{bad}");
    }
}

#[test]
fn a_release_never_offers_a_development_version_and_a_folder_may() {
    let sha = "a".repeat(64);
    let v = format!("{}.4", current());
    let doc = format!(
        r#"{{"version":"{v}","zip":"Seedwell-{v}.zip","sha256":"{sha}","minimum_macos":"15.0","windows":{{"zip":"Seedwell-{v}-windows-x64.zip","sha256":"{sha}","minimum_windows":"10.0"}}}}"#
    );
    let release = parse_latest(doc.as_bytes(), os::RELEASE_SECTION, os::MINIMUM_SYSTEM_KEY, os::release_zip);
    assert!(release.is_none(), "four parts, no tag: not a release");
    let tagged = doc.replacen(&format!(r#""version":"{v}""#), &format!(r#""version":"{v}","tag":"v{v}""#), 1);
    assert!(parse_latest(tagged.as_bytes(), os::RELEASE_SECTION, os::MINIMUM_SYSTEM_KEY, os::release_zip).is_none());
    let build = parse_build(doc.as_bytes(), os::RELEASE_SECTION, os::MINIMUM_SYSTEM_KEY, os::release_zip).unwrap();
    assert_eq!((build.version.as_str(), build.sha256.as_str()), (v.as_str(), sha.as_str()));
    assert!(build.offered(&current(), os_version()), "newer than the release it builds on");
    assert!(!build.offered(&format!("{}.4", current()), os_version()), "not the running build itself");
    let wrong_tag = doc.replacen(&format!(r#""version":"{v}""#), &format!(r#""version":"{v}","tag":"v9.9.9""#), 1);
    assert!(parse_build(wrong_tag.as_bytes(), os::RELEASE_SECTION, os::MINIMUM_SYSTEM_KEY, os::release_zip).is_none());
}

#[test]
fn installing_a_build_needs_the_switch_and_a_real_build_of_another_version() {
    let (core, dir) = mock();
    let u = updates(&core, true, "http://127.0.0.1:9", T0);
    let zip = dir.path().join(os::release_zip(&format!("{}.1", current())));
    std::fs::write(&zip, b"zip").unwrap();
    let e = u.install_build_at(&zip, None, T0).unwrap_err();
    assert!(e.contains("Settings › General › Updates › Allow development builds"), "{e}");
    assert!(core.builds.get().is_some(), "the control server finds the updater");
    let refused = core.builds.get().unwrap().watch(Some(dir.path())).unwrap_err();
    assert!(refused.contains("Allow development builds"), "{refused}");
    set_dev(&core, true, None);
    let other = dir.path().join("build.zip");
    std::fs::write(&other, b"zip").unwrap();
    assert!(u.install_build_at(&other, None, T0).unwrap_err().contains("is not a build"));
    assert!(u.install_build_at(&dir.path().join(os::release_zip("9.9.9")), None, T0).unwrap_err().contains("no such file"));
    let same = dir.path().join(os::release_zip(&current()));
    std::fs::write(&same, b"zip").unwrap();
    assert!(u.install_build_at(&same, None, T0).unwrap_err().contains("already running"));
    let e = u.install_build_at(&zip, Some(&"0".repeat(64)), T0).unwrap_err();
    assert!(e.contains("nothing was installed"), "{e}");
    assert!(u.install_build_at(&zip, None, T0).unwrap_err().contains("no installer"), "this updater was given none");
    assert_eq!(u.model(T0).install_build.map(|b| b.title), Some("Install Build…".into()));
}

#[cfg(unix)]
#[test]
fn a_handed_over_build_runs_the_apps_installer_with_its_checksum_and_the_state_folder() {
    let (core, dir) = mock();
    set_dev(&core, true, None);
    let (script, ran) = fake_installer(dir.path());
    let u = dev_updates(&core, 1, "http://127.0.0.1:9", &script, T0);
    let v = format!("{}.2", current());
    let zip = dir.path().join(os::release_zip(&v));
    std::fs::write(&zip, b"the build").unwrap();
    // The running build started fine: it is the one that comes back.
    u.started();
    let note = u.install_build_at(&zip, None, T0).unwrap();
    assert!(note.starts_with(&format!("Installing {v} (development build)")), "{note}");
    assert!(note.contains(&format!("{}.1 comes back", current())), "{note}");
    let args = wait_for(&ran);
    let sha = sha256_of(&zip).unwrap();
    assert_eq!(args, format!("--zip\n{}\n--sha256\n{sha}\n{}\n", zip.display(), dir.path().display()));
    assert_eq!(u.model(T0).status, format!("Installing {v} (development build)…"));
    assert!(u.install_build_at(&zip, None, T0).unwrap_err().contains("already running"));
    // The next launch is the old build (the new one did not start): it says so and never takes that build again.
    let back = dev_updates(&core, 1, "http://127.0.0.1:9", &script, T0 + MIN);
    assert_eq!(back.model(T0 + MIN).status, format!("{v} didn't start; back on {}.1", current()));
    assert!(back.model(T0 + MIN).show_log.is_some());
    assert_eq!(back.lock().saved.failed, vec![v]);
}

#[cfg(unix)]
#[test]
fn a_watched_folder_installs_a_newer_build_as_it_appears_and_not_a_failed_one() {
    let (core, dir) = mock();
    idle(&core);
    let (script, ran) = fake_installer(dir.path());
    let builds = dir.path().join("builds");
    std::fs::create_dir(&builds).unwrap();
    set_dev(&core, true, Some(&builds));
    let u = dev_updates(&core, 1, "http://127.0.0.1:9", &script, T0);
    assert_eq!(u.next_due_ms(T0), None, "no timer: the watcher reads the folder");
    assert_eq!(u.model(T0).watching, Some(builds.display().to_string()));
    let put = |n: u32| {
        let v = format!("{}.{n}", current());
        let zip = builds.join(os::release_zip(&v));
        std::fs::write(&zip, format!("build {n}")).unwrap();
        let sha = sha256_of(&zip).unwrap();
        let doc = format!(
            r#"{{"version":"{v}","zip":"{}","sha256":"{sha}","minimum_macos":"15.0","windows":{{"zip":"{}","sha256":"{sha}","minimum_windows":"10.0"}}}}"#,
            os::release_zip(&v),
            os::release_zip(&v)
        );
        std::fs::write(builds.join("latest.json.tmp"), doc).unwrap();
        std::fs::rename(builds.join("latest.json.tmp"), builds.join("latest.json")).unwrap();
        (v, zip)
    };
    // An older build is not taken.
    put(1);
    std::thread::sleep(std::time::Duration::from_millis(1500));
    assert!(!ran.exists(), "the running build itself is not installed again");
    let (v, zip) = put(2);
    let args = wait_for(&ran);
    assert!(args.starts_with(&format!("--zip\n{}\n", zip.display())), "{args}");
    // The watcher runs on the real clock.
    assert_eq!(u.model(unix_ms()).status, format!("Installing {v} (development build)…"));
    // It did not start: the next launch remembers it, and the folder does not offer it again.
    std::fs::remove_file(&ran).unwrap();
    drop(u);
    let back = dev_updates(&core, 1, "http://127.0.0.1:9", &script, T0 + MIN);
    std::thread::sleep(std::time::Duration::from_millis(1500));
    assert!(!ran.exists(), "a failed build is not retried");
    assert_eq!(back.model(T0 + MIN).status, format!("{v} didn't start; back on {}.1", current()));
    // Stop Watching stops it.
    back.stop_watching();
    assert_eq!(back.model(T0 + MIN).watching, None);
    put(3);
    std::thread::sleep(std::time::Duration::from_millis(1500));
    assert!(!ran.exists(), "no longer watched");
}

#[test]
fn turning_development_builds_off_while_one_runs_goes_back_to_the_release() {
    let (core, dir) = mock();
    set_dev(&core, true, None);
    let repo = serve(200, latest(&current()));
    let u = dev_updates(&core, 3, &repo, dir.path(), T0);
    assert_eq!(u.model(T0).version, format!("{}.3 (development build)", current()));
    assert_ne!(u.next_due_ms(T0), Some(0), "nothing to do while the switch is on");
    set_dev(&core, false, None);
    assert_eq!(u.next_due_ms(T0 + 1), Some(0), "the way back is checked at once");
    u.check(true, T0 + 1);
    let m = u.model(T0 + 1);
    assert_eq!(m.available_version, Some(current()), "the release, older than the build");
    assert_eq!(m.install.map(|b| b.title), Some(format!("Install {}", current())));
    assert!(u.confirmed_offer(), "installed by itself once nothing downloads, whatever the install switch says");
    // It survives a relaunch before the install, and turning the switch on again ends it.
    let again = dev_updates(&core, 3, &repo, dir.path(), T0 + MIN);
    assert!(again.lock().saved.back_to_release_ms.is_some());
    set_dev(&core, true, None);
    again.next_due_ms(T0 + MIN);
    assert!(again.lock().saved.back_to_release_ms.is_none());
    // Releases newer than a build are offered as usual; older ones are not, with the switch never turned off.
    let (core, dir) = mock();
    let u = dev_updates(&core, 3, &serve(200, latest(&current())), dir.path(), T0);
    u.check(false, T0);
    assert_eq!(u.available_version(), None);
    let u = dev_updates(&core, 3, &serve(200, latest(&newer())), dir.path(), T0);
    u.check(false, T0);
    assert_eq!(u.available_version(), Some(newer()));
}

/// `update.dev_builds` (an agent's `seedwell update dev-builds on|off`) is the Settings switch: it saves the same
/// `[updates] dev_builds`, and off while a development build runs starts the same way back to the release.
#[test]
fn the_agents_dev_builds_switch_is_the_settings_switch() {
    let (core, dir) = mock();
    let u = dev_updates(&core, 3, &serve(200, latest(&current())), dir.path(), T0);
    let hook = core.builds.get().expect("the control server finds the updater");
    assert_eq!(hook.set_dev_builds(true).unwrap(), "Development builds are allowed now: `seedwell update install <zip>` (or a watched folder) installs one");
    assert!(core.settings_load().updates.dev_builds && hook.status().dev_builds, "saved where the switch saves it");
    assert_eq!(hook.set_dev_builds(true).unwrap(), "Development builds were already allowed");
    assert!(u.lock().saved.back_to_release_ms.is_none());
    assert_ne!(u.next_due_ms(T0), Some(0), "nothing to do while the switch is on");
    let note = hook.set_dev_builds(false).unwrap();
    assert!(note.contains("goes back to the latest release"), "{note}");
    assert!(!core.settings_load().updates.dev_builds && !hook.status().dev_builds);
    assert!(u.lock().saved.back_to_release_ms.is_some(), "the way back, as the switch starts it");
    assert_eq!(u.next_due_ms(T0 + 1), Some(0), "checked at once");
    u.check(true, T0 + 1);
    assert_eq!(u.model(T0 + 1).available_version, Some(current()), "the release, older than the build");
    assert!(u.confirmed_offer(), "installed by itself once nothing downloads");
    // On a release, off is only off.
    let (core, _dir) = mock();
    let _u = updates(&core, true, "http://127.0.0.1:9", T0);
    set_dev(&core, true, None);
    assert_eq!(core.builds.get().unwrap().set_dev_builds(false).unwrap(), "Development builds are off");
    assert!(!core.settings_load().updates.dev_builds);
}

#[test]
fn started_marks_the_version_and_three_launches_without_it_go_back_to_the_kept_app() {
    let (core, dir) = mock();
    let state = dir.path().display().to_string();
    let dev = format!("{}.5", current());
    let relaunch = vec!["--background".to_owned()];
    // Nothing kept: launches are counted, nothing else.
    for _ in 0..4 {
        assert_eq!(launch_guard(state.clone(), dev.clone(), "/Applications".into(), relaunch.clone()), None);
    }
    // A clean quit (the app's own, SIGTERM included) does not count, started or not.
    assert_eq!(launch_guard(state.clone(), dev.clone(), "/Applications".into(), relaunch.clone()), None);
    let u = dev_updates(&core, 5, "http://127.0.0.1:9", dir.path(), T0);
    assert!(u.lock().saved.unstarted > 0);
    u.quitting();
    assert_eq!(u.lock().saved.unstarted, 0);
    u.started();
    assert_eq!(std::fs::read_to_string(dir.path().join("launch-ok")).unwrap(), format!("{dev}\n"));
    assert_eq!(u.lock().saved.unstarted, 0);
    drop(u);
    // An app kept to go back to (the installer writes it).
    let kept = dir.path().join("previous");
    std::fs::create_dir_all(&kept).unwrap();
    std::fs::write(kept.join("Seedwell.zip"), b"zip").unwrap();
    std::fs::write(kept.join(os::INSTALLER), b"#").unwrap();
    std::fs::write(kept.join("previous.json"), format!(r#"{{"version":"{}.4"}}"#, current())).unwrap();
    assert_eq!(launch_guard(state.clone(), dev.clone(), "/Applications".into(), relaunch.clone()), None);
    assert_eq!(launch_guard(state.clone(), dev.clone(), "/Applications".into(), relaunch.clone()), None);
    let c = launch_guard(state.clone(), dev.clone(), "/Applications".into(), relaunch.clone()).expect("the third goes back");
    assert!(c.argv.iter().any(|a| a.ends_with(os::INSTALLER)) && c.argv.iter().any(|a| a.contains("Rollback") || a == "--rollback"), "{c:?}");
    assert!(c.env.iter().any(|e| e.name == "SEEDWELL_INSTALL_STATE_DIR" && e.value == state));
    assert_eq!(launch_guard(state.clone(), dev.clone(), "/Applications".into(), relaunch.clone()), None, "counted from zero again");
    // A release never goes back by itself.
    for _ in 0..4 {
        assert_eq!(launch_guard(state.clone(), current(), "/Applications".into(), relaunch.clone()), None);
    }
    // The recovery window: back to the kept app, or the release.
    let r = recovery_actions(state.clone(), "/Applications".into(), Some("http://127.0.0.1:9".into()), relaunch.clone());
    assert_eq!(r.iter().map(|a| a.title.as_str()).collect::<Vec<_>>(), [format!("Go Back to {}.4", current()).as_str(), "Install the Release"]);
    let all = r[1].command.argv.join(" ") + &r[1].command.env.iter().map(|e| e.value.clone()).collect::<String>();
    assert!(all.contains(&format!("http://127.0.0.1:9/releases/latest/download/{}", os::INSTALLER)), "{all}");
    std::fs::remove_dir_all(&kept).unwrap();
    let r = recovery_actions(state, "/Applications".into(), None, relaunch);
    assert_eq!(r.iter().map(|a| a.title.as_str()).collect::<Vec<_>>(), ["Install the Release"]);
}

#[test]
fn a_development_version_that_is_not_this_releases_build_is_ignored() {
    let (core, dir) = mock();
    let u = Updates::new(core.clone(), true, None, os_version().into(), Some("9.9.9.1".into()), String::new(), String::new(), vec![], T0);
    assert_eq!(u.current_version(), current(), "a build names the release it builds on");
    let u = dev_updates(&core, 2, "http://127.0.0.1:9", dir.path(), T0);
    assert_eq!(u.current_version(), format!("{}.2", current()));
}


#[test]
fn the_way_back_to_the_release_is_checked_at_once_then_backs_off_like_any_check() {
    let (core, dir) = mock();
    set_dev(&core, true, None);
    let u = dev_updates(&core, 3, &serve(500, String::new()), dir.path(), T0);
    // An automatic check a moment ago, before the switch: it does not hold the way back.
    u.check(true, T0);
    set_dev(&core, false, None);
    assert_eq!(u.next_due_ms(T0 + 1), Some(0));
    u.check(true, T0 + 1);
    // Two failures in a row (one before the switch): two hours, as for any check that keeps failing.
    assert_eq!(u.next_due_ms(T0 + 1), Some(2 * HOUR), "failed since the switch: backed off");
    u.check(true, T0 + 2 * HOUR + 1);
    assert_eq!(u.next_due_ms(T0 + 2 * HOUR + 1), Some(4 * HOUR), "then doubling");
}
