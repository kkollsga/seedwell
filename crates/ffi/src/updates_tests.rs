use super::*;
use crate::CoreOptions;
use std::io::{Read, Write};
use std::net::TcpListener;

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
    Updates::new(core.clone(), release, Some(repo.to_owned()), os_version().into(), now)
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
    let l = |v: &str, min| Latest { version: v.into(), notes_url: None, minimum_system: min };
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
    let u = Updates::new(core, true, None, os_version().into(), T0);
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
    assert_eq!((m.check_hint.as_str(), m.switches_enabled), ("Development build: checks only when you ask", false));
    assert_eq!((m.check_now.title.as_str(), m.check_now.enabled), ("Check Now", true));
    assert!(m.install.is_none() && m.show_log.is_none() && m.notes_url.is_none());
}
