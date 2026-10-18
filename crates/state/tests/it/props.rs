#![allow(clippy::field_reassign_with_default)]
//! Property tests: config round trip, rule resolution against a reference, tracker matching,
//! codecs, and "never panics on arbitrary input".

use proptest::prelude::*;
use trs_classify::Category;
use trs_state::config::*;
use trs_state::duration::Secs;
use trs_state::record::{FileStamp, PartialPiece, TorrentFlags};
use trs_state::rules::*;
use trs_state::trackers::*;
use trs_state::url::{AnnounceUrl, redact_url};
use trs_state::{Config, Day, Document, ResumeData, TorrentRecord};

// ------------------------------------------------------------------ generators

fn path_s() -> impl Strategy<Value = String> {
    prop_oneof![
        "~/[A-Za-z0-9 _.-]{1,20}(/[A-Za-z0-9 _.-]{1,12}){0,3}",
        "/[A-Za-z0-9 _.-]{1,20}(/[A-Za-z0-9 _.\u{e9}\u{4e2d}-]{1,12}){0,3}",
    ]
}

fn id_s() -> impl Strategy<Value = String> {
    "[a-z0-9][a-z0-9_-]{0,15}".prop_filter("reserved", |s| s != "default" && s != "public" && s != "all" && s != "other-private")
}

fn category() -> impl Strategy<Value = Category> {
    prop::sample::select(Category::ALL.to_vec())
}

fn watch_folder() -> impl Strategy<Value = WatchFolder> {
    (
        path_s(),
        any::<bool>(),
        prop::sample::select(vec![WatchMode::Auto, WatchMode::Ask]),
        prop::sample::select(vec![AfterAdd::Trash, AfterAdd::Leave]),
        prop::option::of(category()),
        prop::option::of(id_s()),
    )
        .prop_map(|(path, enabled, mode, after_add, category, rule)| WatchFolder { path, enabled, mode, after_add, archive_dir: None, category, rule, since: None })
}

fn config_strategy() -> impl Strategy<Value = Config> {
    let general = (any::<[bool; 8]>(), prop::sample::select(vec![OnBattery::KeepGoing, OnBattery::Limit, OnBattery::Pause]));
    let transfers = (
        0..=MAX_RATE,
        0..=MAX_RATE,
        (any::<bool>(), 0..=MAX_RATE, 0..=MAX_RATE),
        (any::<bool>(), 1u8..=127, 0u16..1440, 1u16..1440),
        1u32..=100,
        1u32..=16,
        prop::sample::select(vec![AskBeforeAdding::Never, AskBeforeAdding::MultiFile, AskBeforeAdding::Always]),
    );
    let folders = (
        path_s(),
        path_s(),
        prop::option::of(path_s()),
        any::<bool>(),
        prop::collection::vec(watch_folder(), 0..5),
        (prop::collection::vec(prop::option::of(path_s()), Category::ALL.len()), prop::collection::vec(prop::option::of(path_s()), Category::ALL.len()), 0u32..=100_000, 0u32..=100_000),
    );
    let network = (
        prop_oneof![Just(0u16), 1024u16..=65535],
        any::<[bool; 8]>(),
        prop::sample::select(vec![Encryption::Off, Encryption::Prefer, Encryption::Require]),
        1u32..=10_000,
    );
    let agents = (prop::sample::select(vec![Permission::Off, Permission::ReadOnly, Permission::Control, Permission::Full]), any::<bool>(), 1u32..=1000);
    let advanced = (
        (10u64..=86_400, 10u64..=86_400, 1u64..=3600, 1u32..=1000),
        (1u32..=4096, 16u32..=8192, 0u32..=256, 1u32..=256),
        (any::<bool>(), any::<bool>(), any::<bool>(), prop::sample::select(vec![Preallocation::Sparse, Preallocation::Full])),
        (prop::sample::select(vec![LogLevel::Error, LogLevel::Warn, LogLevel::Info, LogLevel::Debug, LogLevel::Trace]), 1u32..=1024, 1u32..=100, prop::option::of(path_s())),
        (0.5f64..=1.0, any::<bool>(), any::<bool>()),
    );
    (general, transfers, folders, network, agents, advanced).prop_map(|(g, t, f, n, a, adv)| {
        let mut c = Config::default();
        c.general = General {
            launch_at_login: g.0[0],
            notify_finished: g.0[1],
            notify_errors: g.0[2],
            notify_goal: g.0[3],
            menu_bar_show_speed: g.0[4],
            menu_bar_split_by_tracker: g.0[5],
            prevent_sleep_while_downloading: g.0[6],
            on_battery: g.1,
            play_with: "auto".to_owned(),
        };
        c.transfers.download_limit = t.0;
        c.transfers.upload_limit = t.1;
        c.transfers.alt = AltSpeed {
            enabled: t.2.0,
            download_limit: t.2.1,
            upload_limit: t.2.2,
            schedule: Schedule {
                enabled: t.3.0,
                days: DaySet(t.3.1),
                from: TimeOfDay(t.3.2),
                to: TimeOfDay(if t.3.3 == t.3.2 { (t.3.3 + 1) % 1440 } else { t.3.3 }),
            },
        };
        c.transfers.max_active_downloads = t.4;
        c.transfers.max_connections_per_host = t.5;
        c.transfers.ask_before_adding = t.6;
        c.folders.download_dir = f.0;
        c.folders.incomplete_dir = f.1;
        c.folders.by_status = f.3;
        let mut seen = std::collections::HashSet::new();
        c.folders.watch = f.4.into_iter().filter(|w| seen.insert(w.path.clone())).collect();
        for (i, cat) in Category::ALL.into_iter().enumerate() {
            let o = c.folders.categories.get_mut(cat);
            o.incomplete_dir = f.5.0[i].clone();
            o.finished_dir = f.5.1[i].clone();
        }
        c.transfers.large_torrent_gb = f.5.2;
        c.advanced.move_limit_mib = f.5.3;
        c.network.listen_port = n.0;
        c.network.dht = n.1[0];
        c.network.pex = n.1[1];
        c.network.lsd = n.1[2];
        c.network.upnp = n.1[3];
        c.network.natpmp = n.1[4];
        c.network.utp = n.1[5];
        c.network.ipv6 = n.1[6];
        c.network.allow_lan_trackers = n.1[7];
        c.network.encryption = n.2;
        c.network.max_connections = n.3;
        c.network.max_connections_per_torrent = 1 + n.3 / 2;
        c.agents = Agents { permission: a.0, notify_changes: a.1, rate_limit_per_sec: a.2 };
        c.advanced.tiers = Tiers { hot_to_warm: Secs(adv.0.0), warm_to_cold: Secs(adv.0.1), promote_upload_after: Secs(adv.0.2), promote_interested_peers: adv.0.3 };
        c.advanced.write_cache_mib = adv.1.0;
        c.advanced.fd_limit = adv.1.1;
        c.advanced.peers_cache_per_torrent = adv.1.2;
        c.advanced.db_cache_mib = adv.1.3;
        c.advanced.verify_on_completion = adv.2.0;
        c.advanced.background_scrub = adv.2.1;
        c.advanced.scrub_on_ac_only = adv.2.2;
        c.advanced.preallocation = adv.2.3;
        c.advanced.logging = Logging { level: adv.3.0, max_file_mib: adv.3.1, keep_files: adv.3.2, dir: adv.3.3 };
        c.detection = Detection { skip_junk: adv.4.1, ask_when_unsure: adv.4.2, confidence_threshold: adv.4.0, online_lookup: false };
        c
    })
}

fn goal() -> impl Strategy<Value = SeedingGoal> {
    let ratio = 0.01f64..1000.0;
    let time = (1u64..=36_500 * 86_400).prop_map(Secs);
    prop_oneof![
        Just(SeedingGoal::StopWhenFinished),
        Just(SeedingGoal::Forever),
        ratio.clone().prop_map(|ratio| SeedingGoal::Ratio { ratio }),
        time.clone().prop_map(|time| SeedingGoal::Time { time }),
        (ratio.clone(), time.clone()).prop_map(|(ratio, time)| SeedingGoal::RatioOrTime { ratio, time }),
        (ratio, time).prop_map(|(ratio, time)| SeedingGoal::RatioAndTime { ratio, time }),
    ]
}

const TRACKERS: [&str; 4] = ["a", "b", "c", "public"];

fn rule_strategy(idx: usize) -> impl Strategy<Value = Rule> {
    (
        prop::collection::vec(prop::sample::select(TRACKERS.to_vec()), 0..3),
        prop::option::of(any::<bool>()),
        prop::collection::vec(category(), 0..3),
        any::<bool>(),
        prop::option::of(goal()),
        prop::option::of(prop::sample::select(vec![Priority::Low, Priority::Normal, Priority::High])),
        prop::option::of(prop::sample::select(vec![OnGoal::StopKeep, OnGoal::RemoveKeepFiles, OnGoal::RemoveDeleteFiles])),
        prop::option::of((0u64..5).prop_map(Secs::hours)),
        prop::option::of(path_s()),
        prop::option::of(0u64..1_000_000),
    )
        .prop_map(move |(tr, private, cats, enabled, goal, priority, on_goal, min_seed_time, finished_dir, up_limit)| {
            let mut tracker: Vec<String> = tr.into_iter().map(String::from).collect();
            tracker.dedup();
            let mut category = cats;
            category.dedup();
            Rule { id: format!("r{idx}"), name: format!("Rule {idx}"), enabled, when: Match { tracker, private, category }, goal, min_seed_time, stop_when_idle: None, priority, on_goal, finished_dir, up_limit }
        })
}

fn ruleset_strategy() -> impl Strategy<Value = RuleSet> {
    (0usize..7).prop_flat_map(|n| {
        let rules: Vec<_> = (0..n).map(rule_strategy).collect();
        (rules, goal(), prop::sample::select(vec![Priority::Low, Priority::Normal, Priority::High]), prop::option::of(1u64..10_000))
    })
    .prop_map(|(rules, g, p, up)| {
        RuleSet { default: DefaultRule { goal: g, priority: p, up_limit: up, ..DefaultRule::default() }, rules, ..RuleSet::default() }
    })
}

// ------------------------------------------------------------------ properties

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn config_roundtrips_exactly(c in config_strategy()) {
        prop_assert!(c.validate().is_empty(), "{:?}", c.validate());
        let text = c.render().unwrap();
        let back = Config::parse_str(&text).unwrap();
        prop_assert!(back.warnings.is_empty(), "{:?}", back.warnings);
        prop_assert_eq!(&back.value, &c);
        // And the text is a fixed point: saving what we loaded changes nothing.
        prop_assert_eq!(back.value.render().unwrap(), text);
    }

    #[test]
    fn config_unknown_keys_survive(c in config_strategy(), key in "x_[a-z]{3,8}", val in any::<i32>()) {
        let mut text = c.render().unwrap();
        text.push_str(&format!("\n[{key}]\nvalue = {val}\n"));
        let p = Config::parse_str(&text).unwrap();
        prop_assert_eq!(p.warnings.len(), 1);
        let again = Config::parse_str(&p.value.render().unwrap()).unwrap();
        prop_assert_eq!(again.warnings.len(), 1, "unknown table dropped on save");
    }

    #[test]
    fn ruleset_roundtrips_exactly(rs in ruleset_strategy()) {
        prop_assert!(rs.validate().is_empty(), "{:?}", rs.validate());
        let text = rs.render().unwrap();
        let back = RuleSet::parse_str(&text).unwrap().value;
        prop_assert_eq!(&back, &rs);
    }

    #[test]
    fn resolve_is_pure_first_match_and_total(
        rs in ruleset_strategy(),
        tracker in prop::option::of(prop::sample::select(vec!["a", "b", "c", "zzz"])),
        linked in prop::option::of(prop::sample::select(vec!["r0", "r1", "r2", "nope"])),
        private in any::<bool>(),
        cat in category(),
    ) {
        let a = TorrentAttrs { tracker, linked_rule: linked, private, category: cat };
        let p1 = rs.resolve(&a);
        prop_assert_eq!(&p1, &rs.resolve(&a), "pure");
        // Independent oracle.
        let matches = |r: &Rule, waive_tracker: bool| {
            r.enabled
                && (waive_tracker || r.when.tracker.is_empty() || r.when.tracker.iter().any(|t| match tracker { Some(x) => t == x, None => t == "public" }))
                && r.when.private.is_none_or(|p| p == private)
                && (r.when.category.is_empty() || r.when.category.contains(&cat))
        };
        let linked_hit = linked.and_then(|l| rs.rules.iter().find(|r| r.id == l && matches(r, true)));
        let winner = linked_hit.or_else(|| rs.rules.iter().find(|r| matches(r, false)));
        match (winner, &p1.source) {
            (None, PolicySource::Default) => {}
            (Some(w), PolicySource::Rule(id)) => prop_assert!(linked_hit.is_none() && &w.id == id),
            (Some(w), PolicySource::LinkedRule(id)) => prop_assert!(linked_hit.is_some() && &w.id == id),
            other => prop_assert!(false, "oracle disagrees: {:?}", other),
        }
        let d = &rs.default;
        prop_assert_eq!(p1.goal, winner.and_then(|w| w.goal).unwrap_or(d.goal));
        prop_assert_eq!(p1.priority, winner.and_then(|w| w.priority).unwrap_or(d.priority));
        prop_assert_eq!(p1.on_goal, winner.and_then(|w| w.on_goal).unwrap_or(d.on_goal));
        prop_assert_eq!(&p1.finished_dir, &winner.and_then(|w| w.finished_dir.clone()).or_else(|| d.finished_dir.clone()));
        prop_assert_eq!(p1.up_limit, winner.and_then(|w| w.up_limit).or(d.up_limit).filter(|v| *v != 0));
        // Ratio/time evaluation never panics and the hit-and-run floor holds.
        if let Some(m) = p1.min_seed_time {
            prop_assert!(!p1.goal_reached(1e12, Secs(m.0.saturating_sub(1))) || m.0 == 0);
        }
    }

    #[test]
    fn schedule_matches_the_clock(from in 0u16..1440, to in 0u16..1440, minute in 0u16..1440, day in 0u8..7) {
        prop_assume!(from != to);
        let s = Schedule { enabled: true, days: DaySet::EVERY_DAY, from: TimeOfDay(from), to: TimeOfDay(to) };
        let want = if from < to { minute >= from && minute < to } else { minute >= from || minute < to };
        prop_assert_eq!(s.active_at(day, minute), want);
        let off = Schedule { enabled: false, ..s };
        prop_assert!(!off.active_at(day, minute));
    }

    #[test]
    fn duration_text_roundtrips(n in 0u64..=trs_state::duration::MAX_SECS) {
        prop_assert_eq!(Secs::parse(&Secs(n).to_string()), Ok(Secs(n)));
    }

    #[test]
    fn day_arithmetic(secs in 0i64..4_000_000_000, off in -14 * 3600i32..=14 * 3600) {
        let d = Day::from_unix(secs, off);
        let (y, m, dd) = d.ymd();
        prop_assert_eq!(Day::from_ymd(y, m, dd), Some(d));
        // week_start clamps at the epoch, and 1970-01-01 was a Thursday.
        // Days 0-3 (Thu 1 to Sun 4 January 1970) have no Monday on or after the epoch.
        if d.0 < 4 {
            prop_assert_eq!(d.week_start(), Day(0));
        } else {
            prop_assert_eq!(d.week_start().weekday(), 0);
        }
        prop_assert!(d.week_start() <= d && d.month_start() <= d);
        prop_assert_eq!(d.month_start().ymd().2, 1);
        prop_assert!(Day::from_unix(secs + 1, off) >= d);
        prop_assert_eq!(d.weekday(), ((d.0 as u64 + 3) % 7) as u8);
    }
}

// ------------------------------------------------------------------ tracker matching

fn url_s() -> impl Strategy<Value = String> {
    (
        prop::sample::select(vec!["http", "https", "udp"]),
        prop::sample::select(vec!["t.a.org", "tracker.a.org", "t.b.net", "x.c.io", "a.org"]),
        prop::option::of(prop::sample::select(vec![80u16, 443, 6969, 8080])),
        prop::collection::vec(prop::sample::select(vec!["announce", "a", "b", "0123456789abcdef0123456789abcdef"]), 0..3),
        prop::option::of("[a-z]{3,6}=[A-Za-z0-9]{4,12}"),
    )
        .prop_map(|(scheme, host, port, path, query)| {
            let mut s = format!("{scheme}://{host}");
            if let Some(p) = port {
                s.push_str(&format!(":{p}"));
            }
            for seg in path {
                s.push('/');
                s.push_str(seg);
            }
            if let Some(q) = query {
                s.push('?');
                s.push_str(&q);
            }
            s
        })
}

fn pattern_s() -> impl Strategy<Value = String> {
    prop_oneof![
        prop::sample::select(vec!["t.a.org", "*.a.org", "a.org", "t.b.net", "x.c.io", "t.a.org/announce", "tracker.a.org/a/*/announce", "https://t.a.org/announce", "udp://x.c.io:6969"]).prop_map(String::from),
        url_s(),
    ]
}

fn profiles_strategy() -> impl Strategy<Value = TrackerProfiles> {
    prop::collection::vec(prop::collection::vec(pattern_s(), 1..4), 0..5).prop_map(|all| {
        let mut t = TrackerProfiles::default();
        for (i, pats) in all.iter().enumerate() {
            let urls = pats.iter().filter_map(|p| UrlPattern::parse(p).ok()).collect::<Vec<_>>();
            if urls.is_empty() {
                continue;
            }
            t.add(Profile { id: format!("p{i}"), name: format!("P{i}"), urls, ..Profile::default() }).unwrap();
        }
        t
    })
}

proptest! {
    #![proptest_config(ProptestConfig { failure_persistence: None, ..ProptestConfig::default() })]
    #[test]
    fn tracker_match_is_deterministic_and_tier_optimal(t in profiles_strategy(), urls in prop::collection::vec(url_s(), 0..4)) {
        let refs: Vec<&str> = urls.iter().map(String::as_str).collect();
        let m1 = t.match_announce(refs.iter().copied()).map(|m| (m.profile.id.clone(), m.tier, m.url_index));
        let m2 = t.match_announce(refs.iter().copied()).map(|m| (m.profile.id.clone(), m.tier, m.url_index));
        prop_assert_eq!(&m1, &m2);
        // The winner's tier is the best tier any single profile achieves on its own.
        let best = t.profiles.iter().filter_map(|p| {
            let mut solo = TrackerProfiles::default();
            solo.add(p.clone()).ok()?;
            solo.match_announce(refs.iter().copied()).map(|m| m.tier)
        }).min();
        prop_assert_eq!(m1.as_ref().map(|m| m.1), best);
        // Reversing the URL list can change which URL matched but never the best tier.
        let rev: Vec<&str> = refs.iter().rev().copied().collect();
        prop_assert_eq!(t.match_announce(rev).map(|m| m.tier), best);
        // Inserting garbage URLs changes nothing.
        let mut noisy = vec!["", "not a url", "http://"];
        noisy.extend(refs.iter().copied());
        prop_assert_eq!(t.match_announce(noisy).map(|m| (m.profile.id.clone(), m.tier)), m1.map(|m| (m.0, m.1)));
    }

    #[test]
    fn urls_never_leak_their_query(url in url_s(), secret in "[A-Za-z0-9]{12,24}") {
        let with_secret = format!("{url}{}passkey={secret}", if url.contains('?') { "&" } else { "?" });
        if let Ok(u) = AnnounceUrl::parse(&with_secret) {
            let shown = format!("{u} {u:?}");
            prop_assert!(!shown.contains(&secret), "{shown}");
            prop_assert!(shown.contains("?***"));
        }
        prop_assert!(!redact_url(&with_secret).contains(&secret));
        if let Ok(p) = UrlPattern::parse(&with_secret) {
            let shown = format!("{p} {p:?}");
            prop_assert!(!shown.contains(&secret));
        }
    }

    #[test]
    fn profile_files_roundtrip(t in profiles_strategy()) {
        let text = t.render().unwrap();
        let back = TrackerProfiles::parse_str(&text).unwrap().value;
        prop_assert_eq!(back, t);
    }
}

// ------------------------------------------------------------------ never panic

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn parsers_never_panic_on_arbitrary_text(s in "\\PC{0,400}") {
        let _ = Config::parse_str(&s);
        let _ = RuleSet::parse_str(&s);
        let _ = TrackerProfiles::parse_str(&s);
        let _ = AnnounceUrl::parse(&s);
        let _ = UrlPattern::parse(&s);
        let _ = Secs::parse(&s);
        let _ = TimeOfDay::parse(&s);
    }

    #[test]
    fn parsers_never_panic_on_structured_noise(
        key in "[a-z_]{1,12}", sub in "[a-z_]{1,12}", val in prop_oneof![
            any::<i64>().prop_map(|v| v.to_string()),
            any::<f64>().prop_map(|v| format!("{v:?}")),
            "\"[ -~]{0,30}\"".prop_map(String::from),
            Just("[]".to_string()), Just("{}".to_string()), Just("true".to_string()),
        ],
    ) {
        for shape in [format!("[{key}]\n{sub} = {val}\n"), format!("{key} = {val}\n"), format!("[[{key}]]\n{sub} = {val}\n"), format!("[general]\n{sub} = {val}\n[network]\n{key} = {val}\n")] {
            let _ = Config::parse_str(&shape);
            let _ = RuleSet::parse_str(&shape);
            let _ = TrackerProfiles::parse_str(&shape);
        }
        for shape in [
            format!("[[rule]]\nid = \"a\"\nname = \"a\"\n{sub} = {val}\n"),
            format!("[default]\ngoal = {{ kind = \"ratio\", ratio = {val} }}\n"),
            format!("[[rule]]\nid = \"a\"\nname = \"a\"\nmatch = {{ {sub} = {val} }}\n"),
        ] {
            let _ = RuleSet::parse_str(&shape);
        }
        let _ = TrackerProfiles::parse_str(&format!("[[tracker]]\nid = \"a\"\nname = \"a\"\nurls = [{val}]\n"));
    }

    #[test]
    fn mutated_valid_documents_never_panic(c in config_strategy(), pos in any::<prop::sample::Index>(), byte in any::<u8>(), cut in any::<prop::sample::Index>()) {
        let text = c.render().unwrap();
        let mut b = text.clone().into_bytes();
        let i = pos.index(b.len());
        b[i] = byte;
        let _ = Config::parse_str(&String::from_utf8_lossy(&b));
        let _ = Config::parse_str(&text[..text.floor_char_boundary(cut.index(text.len() + 1))]);
    }

    #[test]
    fn record_decoders_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..300)) {
        let _ = TorrentRecord::decode(&bytes);
        let _ = ResumeData::decode(&bytes);
        let mut v1 = vec![1u8];
        v1.extend_from_slice(&bytes);
        let _ = TorrentRecord::decode(&v1);
        let _ = ResumeData::decode(&v1);
    }
}

fn record_strategy() -> impl Strategy<Value = TorrentRecord> {
    (
        ("[ -~\u{e9}\u{4e2d}]{0,100}", any::<u64>(), any::<u32>(), any::<u32>(), any::<i64>()),
        (prop::option::of(any::<i64>()), any::<u16>(), any::<u32>(), prop::option::of("[a-z0-9/ ]{1,40}"), prop::option::of(category())),
        (
            prop::option::of(id_s()),
            prop::option::of(id_s()),
            prop::option::of(prop::sample::select(vec![Priority::Low, Priority::Normal, Priority::High])),
            prop::option::of(goal()),
            prop::option::of(prop::collection::vec(any::<u8>(), 0..64)),
        ),
    )
        .prop_map(|((name, total_size, piece_count, file_count, added_at), (completed_at, flags, save_root, rel, category), (tp, rule, priority, goal, skipped))| TorrentRecord {
            name,
            total_size,
            piece_count,
            file_count,
            added_at,
            completed_at,
            flags: TorrentFlags(flags),
            save_root,
            rel_path: rel.unwrap_or_default(),
            category,
            tracker_profile: tp,
            rule,
            priority,
            goal,
            skipped_files: skipped.map(Vec::into_boxed_slice),
            stage_root: if save_root % 3 == 0 { Some(save_root / 3) } else { None },
            title: if save_root % 2 == 0 { Some(format!("Title {save_root}")) } else { None },
        })
}

fn resume_strategy() -> impl Strategy<Value = ResumeData> {
    (1u32..2000).prop_flat_map(|pieces| {
        let len = (pieces as usize).div_ceil(8);
        (
            Just(pieces),
            prop::collection::vec(any::<u8>(), len),
            prop::collection::btree_set(0..pieces, 0..20),
            prop::collection::vec((any::<u64>(), any::<i64>()), 0..10),
            any::<i64>(),
        )
    })
    .prop_map(|(pieces, mut bits, partial, files, saved_at)| {
        let spare = (bits.len() * 8) as u32 - pieces;
        if spare > 0 {
            let last = bits.len() - 1;
            bits[last] &= !((1u8 << spare) - 1);
        }
        ResumeData {
            piece_count: pieces,
            bitfield: bits.into_boxed_slice(),
            partial: partial.into_iter().map(|index| PartialPiece { index, blocks: vec![index as u8; (index % 5) as usize].into_boxed_slice() }).collect(),
            files: files.into_iter().map(|(size, mtime_ns)| FileStamp { size, mtime_ns }).collect(),
            saved_at,
        }
    })
}

proptest! {
    #![proptest_config(ProptestConfig { failure_persistence: None, ..ProptestConfig::default() })]
    #[test]
    fn torrent_record_roundtrips(r in record_strategy()) {
        let mut r = r;
        r.rel_path.truncate(r.rel_path.len().min(4000));
        let enc = r.encode();
        prop_assert_eq!(TorrentRecord::decode(&enc), Ok(r));
    }

    #[test]
    fn resume_roundtrips(r in resume_strategy()) {
        let enc = r.encode().unwrap();
        prop_assert_eq!(ResumeData::decode(&enc), Ok(r));
    }
}
