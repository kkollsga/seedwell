use std::collections::HashMap;

use super::*;
use crate::chat_text::tests::{channel, topic};
use crate::library::tests::{mock, tracker};
use crate::row_text::tests::row;
use crate::sync::SyncMessageKind;
use crate::types::{Category, PendingStart, TransferState};

const ON: NoticeSettings = NoticeSettings { finished: true, errors: true, goal: true, agents: true };

fn facts() -> Facts {
    Facts { settings: ON, trackers: trackers(), downloading: Vec::new(), queued: Vec::new(), tracker: None, requests: Vec::new() }
}

/// "full" hides everything, "seed" only what is sharing, "shown" and "public" nothing.
fn trackers() -> Vec<TrackerSummary> {
    let hidden = |id: &str, only_seeding: bool| TrackerSummary { hidden: true, hide_only_seeding: only_seeding, ..tracker(id, 1, true) };
    vec![hidden("full", false), hidden("seed", true), tracker("shown", 1, true), tracker("public", 1, false)]
}

fn on(id: &str, tracker: &str) -> TransferRow {
    TransferRow { display_name: format!("{id} name"), tracker_id: tracker.into(), ..row(id, TransferState::Downloading) }
}

fn posted(o: NoticeOutcome) -> Notice {
    match o {
        NoticeOutcome::Post { notice } => notice,
        other => panic!("expected a notice, got {other:?}"),
    }
}

fn run(event: NoticeEvent, f: &Facts) -> NoticeOutcome {
    decide(&mut State::default(), event, f, 0)
}

// MARK: Hidden trackers (SPEC 7E.7)

#[test]
fn a_fully_hidden_tracker_says_nothing_and_hide_only_sharing_keeps_download_notices() {
    let list = trackers();
    assert!(silenced(Some("full"), Side::Download, &list) && silenced(Some("full"), Side::Sharing, &list));
    assert!(!silenced(Some("seed"), Side::Download, &list));
    assert!(silenced(Some("seed"), Side::Sharing, &list));
    for id in [Some("shown"), Some("public"), Some("nope"), None] {
        assert!(!silenced(id, Side::Download, &list) && !silenced(id, Side::Sharing, &list), "{id:?} is announced as usual");
    }
}

// MARK: Each kind

#[test]
fn a_finished_download_names_itself_and_a_click_shows_it() {
    let n = posted(run(NoticeEvent::Finished { row: on("a", "public") }, &facts()));
    assert_eq!((n.kind, n.title.as_str(), n.body.as_str()), (NoticeKind::Finished, "Download finished", "a name"));
    assert_eq!((n.key, n.route.clone()), (None, NoticeRoute::Transfer { id: "a".into() }));
    assert_eq!(notice_route(n.click), n.route);
    // Off in Settings, or a fully hidden tracker: nothing.
    let off = Facts { settings: NoticeSettings { finished: false, ..ON }, ..facts() };
    assert_eq!(run(NoticeEvent::Finished { row: on("a", "public") }, &off), NoticeOutcome::Nothing);
    assert_eq!(run(NoticeEvent::Finished { row: on("a", "full") }, &facts()), NoticeOutcome::Nothing);
    assert!(matches!(run(NoticeEvent::Finished { row: on("a", "seed") }, &facts()), NoticeOutcome::Post { .. }), "a download of hide-only-sharing");
}

#[test]
fn more_than_three_finished_in_ten_seconds_become_one_notice_that_replaces_itself() {
    let f = facts();
    let mut state = State::default();
    let mut at = |ms: u64, id: &str| posted(decide(&mut state, NoticeEvent::Finished { row: on(id, "public") }, &f, ms));
    for (i, ms) in [0, 1_000, 2_000].into_iter().enumerate() {
        assert_eq!(at(ms, &format!("r{i}")).kind, NoticeKind::Finished);
    }
    let burst = at(3_000, "r3");
    assert_eq!((burst.kind, burst.title.as_str(), burst.body.as_str()), (NoticeKind::FinishedBurst, "4 downloads finished", "Open Seedwell to see them."));
    assert_eq!((burst.key.as_deref(), burst.route), (Some("finished-burst"), NoticeRoute::Window));
    assert_eq!(at(9_999, "r4").title, "5 downloads finished", "the same key: it takes the place of the last one");
    // Ten seconds on, the first ones no longer count: 3 000, 9 999 and this one are three, each its own notice.
    assert_eq!(at(12_500, "r5").title, "Download finished");
    assert_eq!(at(30_000, "r6").kind, NoticeKind::Finished, "a quiet spell starts over");
}

#[test]
fn a_finished_download_that_was_held_back_does_not_count_toward_a_burst() {
    let f = facts();
    let mut state = State::default();
    for ms in [0, 10, 20, 30] {
        assert_eq!(decide(&mut state, NoticeEvent::Finished { row: on("h", "full") }, &f, ms), NoticeOutcome::Nothing);
    }
    assert_eq!(posted(decide(&mut state, NoticeEvent::Finished { row: on("a", "public") }, &f, 40)).kind, NoticeKind::Finished);
}

#[test]
fn an_error_names_the_download_or_a_transfer_and_follows_the_side_it_happened_on() {
    let f = Facts { downloading: vec![on("d", "seed")], queued: vec![on("q", "seed")], tracker: Some("seed".into()), ..facts() };
    let n = posted(run(NoticeEvent::Error { id: Some("d".into()), message: "Disk full".into() }, &f));
    assert_eq!((n.kind, n.title.as_str(), n.body.as_str()), (NoticeKind::Error, "Problem with d name", "Disk full"));
    assert_eq!(n.route, NoticeRoute::Transfer { id: "d".into() });
    // A queued download is on the download side too, but only the active list names it.
    assert_eq!(posted(run(NoticeEvent::Error { id: Some("q".into()), message: "x".into() }, &f)).title, "Problem with a transfer");
    // Seeding (in no download list): its tracker hides what is sharing.
    assert_eq!(run(NoticeEvent::Error { id: Some("s".into()), message: "x".into() }, &f), NoticeOutcome::Nothing);
    // No id: the window; no tracker, so nothing hides it.
    let n = posted(run(NoticeEvent::Error { id: None, message: "The state folder is full".into() }, &facts()));
    assert_eq!((n.title.as_str(), n.route), ("Problem with a transfer", NoticeRoute::Window));
    let off = Facts { settings: NoticeSettings { errors: false, ..ON }, ..facts() };
    assert_eq!(run(NoticeEvent::Error { id: None, message: "x".into() }, &off), NoticeOutcome::Nothing);
}

#[test]
fn a_reached_goal_is_a_sharing_notice() {
    let n = posted(run(NoticeEvent::GoalReached { row: on("g", "public") }, &facts()));
    assert_eq!((n.kind, n.title.as_str(), n.body.as_str(), n.route), (NoticeKind::GoalReached, "Seeding goal reached", "g name", NoticeRoute::Window));
    assert_eq!(run(NoticeEvent::GoalReached { row: on("g", "seed") }, &facts()), NoticeOutcome::Nothing);
    let off = Facts { settings: NoticeSettings { goal: false, ..ON }, ..facts() };
    assert_eq!(run(NoticeEvent::GoalReached { row: on("g", "public") }, &off), NoticeOutcome::Nothing);
}

#[test]
fn a_choice_opens_its_chooser_unless_the_starting_notifier_holds_it() {
    let ev = |requested, shown, tracker: &str| NoticeEvent::NeedsChoice { row: on("c", tracker), chooser_requested: requested, starting_shown: shown };
    let n = posted(run(ev(false, false, "public"), &facts()));
    assert_eq!((n.kind, n.title.as_str(), n.body.as_str()), (NoticeKind::NeedsChoice, "Choose what to download", "c name"));
    assert_eq!((n.key.as_deref(), n.route.clone()), (Some("choice-c"), NoticeRoute::Chooser { id: "c".into() }));
    assert_eq!(notice_route(n.click), NoticeRoute::Chooser { id: "c".into() });
    assert_eq!(run(ev(true, false, "public"), &facts()), NoticeOutcome::Open { route: NoticeRoute::Chooser { id: "c".into() } });
    assert_eq!(run(ev(true, false, "full"), &facts()), NoticeOutcome::Open { route: NoticeRoute::Chooser { id: "c".into() } }, "asked for");
    assert_eq!(run(ev(true, true, "public"), &facts()), NoticeOutcome::Nothing, "the popover is the notice");
    assert_eq!(run(ev(false, false, "full"), &facts()), NoticeOutcome::Nothing);
    // Not a setting: a held torrent always says so.
    let off = Facts { settings: NoticeSettings { finished: false, errors: false, goal: false, agents: false }, ..facts() };
    assert!(matches!(run(ev(false, false, "public"), &off), NoticeOutcome::Post { .. }));
}

fn added(destination: Option<String>) -> AutoAdded {
    AutoAdded { id: "n".into(), name: "Night of the Living Dead (1968)".into(), category: Category::Movie, tracker: "tracker-y.net".into(), destination }
}

#[test]
fn an_automatic_add_says_what_where_and_from_which_tracker() {
    let n = posted(run(NoticeEvent::AutoAdded { added: added(Some("/Volumes/Media/Movies".into())) }, &facts()));
    assert_eq!((n.kind, n.title.as_str()), (NoticeKind::Added, "Added Night of the Living Dead (1968)"));
    assert_eq!(n.body, "Movie · tracker-y.net · → /Volumes/Media/Movies");
    assert_eq!(n.route, NoticeRoute::Transfer { id: "n".into() });
    assert_eq!(posted(run(NoticeEvent::AutoAdded { added: added(None) }, &facts())).body, "Movie · tracker-y.net");
    #[cfg(unix)]
    if let Some(home) = trs_platform::dirs::home().filter(|h| h.parent().is_some()) {
        let d = home.join("Media/Movies").display().to_string();
        assert_eq!(posted(run(NoticeEvent::AutoAdded { added: added(Some(d)) }, &facts())).body, "Movie · tracker-y.net · → ~/Media/Movies");
    }
}

#[test]
fn an_automatic_add_that_waits_to_start_or_is_hidden_says_nothing() {
    let mut waiting = on("n", "public");
    waiting.pending = Some(PendingStart { starts_at_ms: 0, remaining_ms: 5_000, paused: false });
    let f = Facts { downloading: vec![waiting], ..facts() };
    assert_eq!(run(NoticeEvent::AutoAdded { added: added(None) }, &f), NoticeOutcome::Nothing, "the starting notifier is its notice");
    let f = Facts { downloading: vec![on("n", "public")], ..facts() };
    assert!(matches!(run(NoticeEvent::AutoAdded { added: added(None) }, &f), NoticeOutcome::Post { .. }), "started at once");
    let f = Facts { tracker: Some("full".into()), ..facts() };
    assert_eq!(run(NoticeEvent::AutoAdded { added: added(None) }, &f), NoticeOutcome::Nothing);
}

fn dup(id: &str, trackers_added: u32) -> NoticeEvent {
    NoticeEvent::Duplicate { id: id.into(), name: "Sintel".into(), trackers_added }
}

#[test]
fn a_duplicate_says_it_was_there_or_how_many_trackers_it_gained() {
    let n = posted(run(dup("s", 0), &facts()));
    assert_eq!((n.kind, n.title.as_str(), n.body.as_str(), n.route), (NoticeKind::Duplicate, "Already in Seedwell", "Sintel", NoticeRoute::Window));
    let n = posted(run(dup("s", 1), &facts()));
    assert_eq!((n.title.as_str(), n.body.as_str()), ("Added 1 tracker to Sintel", "It was already in Seedwell"));
    assert_eq!(posted(run(dup("s", 3), &facts())).title, "Added 3 trackers to Sintel");
}

/// Added again while the first copy is held: the notice says so and opens the starting notifier (a countdown) or the
/// chooser (a choice), one notice per torrent however often it is added.
#[test]
fn a_duplicate_of_a_held_torrent_opens_what_holds_it() {
    let mut counting = on("s", "public");
    counting.pending = Some(PendingStart { starts_at_ms: 0, remaining_ms: 5_000, paused: false });
    let f = Facts { downloading: vec![counting], ..facts() };
    let n = posted(run(dup("s", 0), &f));
    assert_eq!((n.kind, n.title.as_str(), n.body.as_str()), (NoticeKind::Duplicate, "Already in Seedwell", "Sintel is about to start"));
    assert_eq!((n.route.clone(), n.key.as_deref()), (NoticeRoute::Starting { id: "s".into() }, Some("duplicate-s")));
    assert_eq!(notice_route(n.click), n.route, "the click survives a restart");
    let mut choosing = on("s", "public");
    choosing.awaiting_choice = true;
    let f = Facts { queued: vec![choosing], ..facts() };
    let n = posted(run(dup("s", 0), &f));
    assert_eq!((n.body.as_str(), n.route), ("Sintel is waiting for you to choose what to download", NoticeRoute::Chooser { id: "s".into() }));
    // A running one is not held: the usual notice.
    let f = Facts { downloading: vec![on("s", "public")], ..facts() };
    assert_eq!(posted(run(dup("s", 0), &f)).route, NoticeRoute::Window);
}

#[test]
fn an_agent_change_is_announced_when_it_worked_and_the_setting_is_on() {
    let a = |outcome: &str| AgentActivity { at_ms: 0, client: "kglite".into(), method: "add".into(), summary: "Added Sintel".into(), outcome: outcome.into() };
    let n = posted(run(NoticeEvent::Agent { activity: a("ok") }, &facts()));
    assert_eq!((n.kind, n.title.as_str(), n.body.as_str(), n.route), (NoticeKind::Agent, "kglite", "Added Sintel", NoticeRoute::Window));
    assert_eq!(run(NoticeEvent::Agent { activity: a("denied") }, &facts()), NoticeOutcome::Nothing);
    assert_eq!(run(NoticeEvent::Agent { activity: a("error: bad_request") }, &facts()), NoticeOutcome::Nothing);
    let off = Facts { settings: NoticeSettings { agents: false, ..ON }, ..facts() };
    assert_eq!(run(NoticeEvent::Agent { activity: a("ok") }, &off), NoticeOutcome::Nothing);
}

/// An agent's request for access (SPEC 9A.3): once, whatever Notify me says, with Approve and Deny; the user's own
/// answers and expiries post nothing.
#[test]
fn an_agents_request_is_announced_once_with_approve_and_deny() {
    use trs_control::access::{Entry, State as RState, TTL_MS, Want};
    let e = Entry {
        id: "5eed0a11".into(),
        want: Want::Level(trs_control::Permission::Full),
        agents: vec!["Claude".into()],
        why: Some("to install the release".into()),
        at_ms: 0,
        expires_ms: TTL_MS,
        state: RState::Pending,
        decided_ms: None,
        how: None,
        note: None,
        announced: None,
        notify: true,
    };
    let asked = |method: &str| NoticeEvent::Agent {
        activity: AgentActivity { at_ms: 0, client: "Claude".into(), method: method.into(), summary: "ask the user for agent permission full".into(), outcome: "ok".into() },
    };
    // Notify me off: the request is still told.
    let f = Facts { settings: NoticeSettings { agents: false, ..ON }, requests: vec![e.clone()], ..facts() };
    let mut state = State::default();
    let n = posted(decide(&mut state, asked("agents.request"), &f, 0));
    assert_eq!((n.kind, n.title.as_str(), n.route.clone()), (NoticeKind::AgentRequest, "Claude asks for agent permission full", NoticeRoute::AgentRequests));
    assert_eq!(n.body, "“to install the release”\nApprove gives exactly this; Deny refuses. Also in Settings › Agents.");
    assert_eq!((n.key.as_deref(), n.category.as_deref()), (Some("agent-request-5eed0a11"), Some("agent-request")));
    assert_eq!(n.urgency, NoticeUrgency::Persistent, "someone waits for the answer: it stays in view");
    assert_eq!(n.actions.iter().map(|a| (a.id.as_str(), a.title.as_str(), a.destructive, a.authenticate)).collect::<Vec<_>>(), [
        ("approve", "Approve", false, true),
        ("deny", "Deny", true, false)
    ]);
    assert_eq!(notice_categories(), [NoticeCategory { id: "agent-request".into(), actions: n.actions.clone() }]);
    assert_eq!(notice_route(n.click.clone()), NoticeRoute::AgentRequests);
    assert_eq!(notice_decision(n.click.clone(), "approve".into()), Some(NoticeDecision { request_id: "5eed0a11".into(), approve: true }));
    assert_eq!(notice_decision(n.click.clone(), "deny".into()), Some(NoticeDecision { request_id: "5eed0a11".into(), approve: false }));
    for (click, action) in [(n.click.as_str(), "open"), ("transfer\nabc", "approve"), ("agent-request\n", "approve"), ("agent-request\na\nb", "approve"), ("", "approve")] {
        assert_eq!(notice_decision(click.into(), action.into()), None, "{click:?} {action}");
    }
    // Once: the same request again (a second agent joined it) posts nothing.
    assert_eq!(decide(&mut state, asked("agents.request"), &f, 1), NoticeOutcome::Nothing);
    // The user's answers and expiries: nothing, also with Notify me on.
    let on = Facts { requests: vec![e], ..facts() };
    for m in ["agents.approve", "agents.deny", "agents.expire"] {
        assert_eq!(run(asked(m), &on), NoticeOutcome::Nothing, "{m}");
    }
    assert_eq!(run(asked("agents.request"), &facts()), NoticeOutcome::Nothing, "no request the user is asked about");
    // A chat reply's approval that could not be done is told (nobody looks at the app); a button's is the host's.
    let failed = |summary: &str| NoticeEvent::Agent {
        activity: AgentActivity { at_ms: 0, client: "the user".into(), method: "agents.approve".into(), summary: summary.into(), outcome: "error: invalid".into() },
    };
    let n = posted(run(failed("approved Claude's request for agent permission full (with a reply in chat), but agent access is switched off"), &facts()));
    assert_eq!((n.kind, n.title.as_str(), n.route), (NoticeKind::Agent, "Not approved", NoticeRoute::AgentRequests));
    assert_eq!(run(failed("approved Claude's request for agent permission full (in the app), but agent access is switched off"), &facts()), NoticeOutcome::Nothing);
}

// MARK: Clicks

#[test]
fn every_route_survives_as_text_and_anything_else_opens_the_window() {
    let routes = [
        NoticeRoute::Window,
        NoticeRoute::Transfer { id: "abc".into() },
        NoticeRoute::Chooser { id: "abc".into() },
        NoticeRoute::Place { place: LibraryPlace::Chat { channel_id: "C-1".into(), topic_id: None } },
        NoticeRoute::Place { place: LibraryPlace::Chat { channel_id: "C-1".into(), topic_id: Some("T/2".into()) } },
        NoticeRoute::Place { place: LibraryPlace::Direct { device_id: "D9".into() } },
        NoticeRoute::AgentRequests,
    ];
    for r in routes {
        assert_eq!(notice_route(click_of(&r)), r);
    }
    for junk in ["", "transfer", "transfer\n", "chat\n", "something\nelse", "{\"transfer\":\"a\"}"] {
        assert_eq!(notice_route(junk.into()), NoticeRoute::Window, "{junk:?}");
    }
}

// MARK: Chat

fn message(body: &str, is_me: bool) -> SyncMessage {
    SyncMessage {
        id: "m1".into(),
        channel_id: "Team".into(),
        author_device: "D1".into(),
        author_name: "Studio iMac".into(),
        author_user: String::new(),
        is_me,
        agent: None,
        at_s: 0,
        reply_to: None,
        topic: None,
        kind: SyncMessageKind::Text,
        body: body.into(),
        images: Vec::new(),
        files: Vec::new(),
        place: "Team".into(),
    }
}

fn general(id: &str) -> LibraryPlace {
    LibraryPlace::Chat { channel_id: id.into(), topic_id: None }
}

fn conv(place: LibraryPlace, unread: u32, title: &str, muted: bool) -> Conversation {
    Conversation { place, unread, title: title.into(), muted }
}

const AWAY: NoticeView = NoticeView { in_front: false, window_place: None };

/// One poll with these counts, each conversation's newest message from another device.
fn poll(seen: &mut Option<HashMap<LibraryPlace, u32>>, convs: Vec<Conversation>, view: &NoticeView) -> Vec<Notice> {
    chat_step(seen, convs, view, |_| Some(message("Hello there", false)))
}

#[test]
fn the_first_poll_only_takes_the_counts_and_a_new_message_gets_a_banner() {
    let mut seen = None;
    assert!(poll(&mut seen, vec![conv(general("Team"), 4, "Team", false)], &AWAY).is_empty(), "unread before the app ran is no news");
    assert!(poll(&mut seen, vec![conv(general("Team"), 4, "Team", false)], &AWAY).is_empty());
    let n = poll(&mut seen, vec![conv(general("Team"), 5, "Team", false)], &AWAY);
    assert_eq!(n.len(), 1);
    assert_eq!((n[0].kind, n[0].title.as_str(), n[0].body.as_str()), (NoticeKind::ChatMessage, "Team", "Studio iMac: Hello there"));
    assert_eq!(n[0].route, NoticeRoute::Place { place: general("Team") });
    assert_eq!(n[0].key.as_deref(), Some("chat-Team/"));
    assert_eq!(notice_route(n[0].click.clone()), n[0].route);
    assert!(poll(&mut seen, vec![conv(general("Team"), 0, "Team", false)], &AWAY).is_empty(), "read: the count went down");
}

#[test]
fn topics_and_private_conversations_have_their_own_banners() {
    let topic_place = LibraryPlace::Chat { channel_id: "C-Team".into(), topic_id: Some("T1".into()) };
    let direct = LibraryPlace::Direct { device_id: "D1".into() };
    let mut seen = None;
    let convs = |t, d| vec![conv(general("C-Team"), 0, "Team", false), conv(topic_place.clone(), t, "Team › Dinner", false), conv(direct.clone(), d, "Studio iMac", false)];
    poll(&mut seen, convs(0, 0), &AWAY);
    let n = poll(&mut seen, convs(1, 2), &AWAY);
    assert_eq!(n.iter().map(|n| n.title.as_str()).collect::<Vec<_>>(), ["Team › Dinner", "Studio iMac"]);
    assert_eq!(n.iter().map(|n| n.key.clone().unwrap()).collect::<Vec<_>>(), ["chat-C-Team/T1", "direct-D1"]);
    assert_eq!(n[1].route, NoticeRoute::Place { place: direct });
}

#[test]
fn my_own_message_a_muted_channel_and_the_conversation_on_screen_get_none() {
    let mut seen = None;
    poll(&mut seen, vec![conv(general("Team"), 0, "Team", false)], &AWAY);
    let mine = chat_step(&mut seen, vec![conv(general("Team"), 1, "Team", false)], &AWAY, |_| Some(message("from me", true)));
    assert!(mine.is_empty(), "sent from this device (another of my devices' copy counts as read here)");
    assert!(chat_step(&mut seen, vec![conv(general("Team"), 2, "Team", false)], &AWAY, |_| None).is_empty(), "no message to show");

    let front = NoticeView { in_front: true, window_place: Some(general("Team")) };
    assert!(poll(&mut seen, vec![conv(general("Team"), 3, "Team", false)], &front).is_empty(), "the user is looking at it");
    let elsewhere = NoticeView { in_front: true, window_place: Some(LibraryPlace::Downloads) };
    assert_eq!(poll(&mut seen, vec![conv(general("Team"), 4, "Team", false)], &elsewhere).len(), 1);
    let behind = NoticeView { in_front: false, window_place: Some(general("Team")) };
    assert_eq!(poll(&mut seen, vec![conv(general("Team"), 5, "Team", false)], &behind).len(), 1, "another app is in front");

    assert!(poll(&mut seen, vec![conv(general("Team"), 9, "Team", true)], &AWAY).is_empty(), "muted");
    assert!(poll(&mut seen, vec![conv(general("Team"), 9, "Team", false)], &AWAY).is_empty(), "unmuted: the old messages are no news");
    assert_eq!(poll(&mut seen, vec![conv(general("Team"), 10, "Team", false)], &AWAY).len(), 1);
}

#[test]
fn a_channel_joined_since_the_last_poll_announces_its_unread_messages() {
    let mut seen = None;
    poll(&mut seen, Vec::new(), &AWAY);
    assert_eq!(poll(&mut seen, vec![conv(general("New"), 2, "New", false)], &AWAY).len(), 1);
}

#[test]
fn chat_titles_come_from_the_channel_and_topic() {
    assert_eq!(chat_place_title(channel("Team", 2, 1, 0, false), Some(topic("Dinner", "C-Team", 0))), "Team › Dinner");
}

// MARK: The notifier on a core

#[test]
fn the_notifier_reads_the_settings_and_the_library_of_its_core() {
    let (core, _dir) = mock();
    let n = Notices::new(core.clone());
    assert_eq!(n.model().chat_poll_ms, 2_000);
    let dup = posted(n.event(NoticeEvent::Duplicate { id: "nope".into(), name: "Sintel".into(), trackers_added: 0 }));
    assert_eq!(dup.title, "Already in Seedwell");
    // The goal notice is off by default (SPEC §9: optional).
    let goal = core.settings_load().general.notify_goal;
    let out = n.event(NoticeEvent::GoalReached { row: on("g", "public") });
    assert_eq!(matches!(out, NoticeOutcome::Post { .. }), goal);
    // The chat poll: the first one takes the counts.
    let view = NoticeView { in_front: false, window_place: None };
    assert!(n.chat(view.clone()).is_empty());
    assert!(n.chat(view).is_empty(), "nothing new in the mock");
}

// MARK: The settings banner

#[test]
fn the_settings_banner_lists_problems_first_and_the_restart_last() {
    assert_eq!(settings_banner(Vec::new(), Vec::new(), Vec::new(), true), None);
    let note = "config.toml: network.listen_port: 80 is not a valid value (…); the default 0 is used until it is fixed".to_string();
    let b = settings_banner(vec!["Rejected".into()], vec![note.clone(), "Rejected".into()], vec!["uTP".into(), "listening interface".into()], true).unwrap();
    let lines: Vec<_> = b.lines.iter().map(|l| (l.text.as_str(), l.tone)).collect();
    assert_eq!(
        lines,
        [
            ("Rejected", SettingsBannerTone::Problem),
            (note.as_str(), SettingsBannerTone::Problem),
            ("Restart Seedwell to apply: uTP, listening interface", SettingsBannerTone::Restart),
        ]
    );
    assert_eq!(b.action, None, "in Settings already");
    let outside = settings_banner(Vec::new(), vec![note], Vec::new(), false).unwrap();
    assert_eq!(outside.action.map(|a| a.title), Some("Open Settings".into()));
    let restart_only = settings_banner(Vec::new(), Vec::new(), vec!["uTP".into()], false).unwrap();
    assert_eq!(restart_only.action, None, "nothing to fix in Settings");
}

// MARK: Urgency and withdrawal

#[test]
fn only_a_request_asks_to_stay_in_view() {
    let finished = posted(run(NoticeEvent::Finished { row: on("f", "public") }, &facts()));
    assert_eq!(finished.urgency, NoticeUrgency::Normal);
    let dup = posted(run(NoticeEvent::Duplicate { id: "d".into(), name: "Sintel".into(), trackers_added: 0 }, &facts()));
    assert_eq!(dup.urgency, NoticeUrgency::Normal);
}

/// A request's notice goes once the request no longer waits (answered anywhere, expired, withdrawn); other notices and
/// the requests still waiting stay.
#[test]
fn a_request_no_longer_waiting_takes_its_notice_away() {
    use trs_control::access::{Entry, State as RState, TTL_MS, Want};
    let waiting = Entry {
        id: "b2".into(),
        want: Want::DevBuilds,
        agents: vec!["Codex".into()],
        why: None,
        at_ms: 0,
        expires_ms: TTL_MS,
        state: RState::Pending,
        decided_ms: None,
        how: None,
        note: None,
        announced: None,
        notify: true,
    };
    let delivered: Vec<String> = ["agent-request-a1", "agent-request-b2", "chat-C-1/", "finished-burst", "agent-requests"].map(String::from).into();
    assert_eq!(stale_request_keys(&delivered, std::slice::from_ref(&waiting)), ["agent-request-a1"]);
    assert_eq!(stale_request_keys(&delivered, &[]), ["agent-request-a1", "agent-request-b2"]);
    assert!(stale_request_keys(&["chat-C-1/".to_string()], &[]).is_empty());
}

#[test]
fn answering_a_request_takes_its_notice_away() {
    let (core, _dir) = mock();
    let n = Notices::new(core.clone());
    let id = core.agent_requests(0).rows[0].id.clone();
    let key = format!("agent-request-{id}");
    let delivered = vec![key.clone(), "chat-C-1/".into()];
    assert!(n.withdrawn(delivered.clone()).is_empty(), "it still waits");
    assert!(core.agent_request_decide(id, true).ok);
    assert_eq!(n.withdrawn(delivered), [key]);
    assert!(n.withdrawn(Vec::new()).is_empty());
}

fn kept(path: &str, why: &str) -> crate::sync::ondemand::SyncFailed {
    crate::sync::ondemand::SyncFailed { path: path.into(), error: why.into() }
}

fn evicted(kept_why: Vec<crate::sync::ondemand::SyncFailed>, failed: Vec<crate::sync::ondemand::SyncFailed>) -> crate::sync::ondemand::SyncEvicted {
    crate::sync::ondemand::SyncEvicted { files: 2, bytes: 10, kept: kept_why.len() as u64, kept_why, failed }
}

/// "Free up space" that kept files tells the user once per folder, in the core's words; a free that freed everything
/// and the system's own clean-ups say nothing.
#[test]
fn a_free_that_kept_files_says_how_many_and_why() {
    use trs_sync::ondemand::{KEPT_CHANGED, KEPT_PINNED, NOT_HELD_ELSEWHERE};
    let (core, _dir) = mock();
    let n = Notices::new(core.clone());
    let f = core.sync_snapshot().folders[0].clone();

    // Everything freed, or the system's clean-up (Storage Sense, disk pressure): nothing to tell.
    assert_eq!(n.sync_evict_notice(f.id.clone(), evicted(Vec::new(), Vec::new()), true), None);
    assert_eq!(n.sync_evict_notice(f.id.clone(), evicted(vec![kept("a.jpg", KEPT_PINNED)], Vec::new()), false), None);
    assert_eq!(n.sync_evict_notice("not-a-folder".into(), evicted(vec![kept("a.jpg", KEPT_PINNED)], Vec::new()), true), None, "no folder");

    // One file: named.
    let one = n.sync_evict_notice(f.id.clone(), evicted(vec![kept("2026/beach.jpg", KEPT_PINNED)], Vec::new()), true).unwrap();
    assert_eq!(one.kind, NoticeKind::SyncKept);
    assert_eq!(one.title, format!("“beach.jpg” stays on this {}", trs_platform::computer!()));
    assert_eq!(
        one.body,
        format!("It is set to always stay on this {}: stop keeping it here first. (“{}” in Folder Sync)", trs_platform::computer!(), f.name)
    );
    assert_eq!(one.route, NoticeRoute::Place { place: LibraryPlace::FolderSync });
    assert_eq!(notice_route(one.click.clone()), one.route);

    // Several, for several reasons: counted per reason.
    let many = n
        .sync_evict_notice(
            f.id.clone(),
            evicted(vec![kept("a", KEPT_PINNED), kept("b", KEPT_PINNED), kept("c", NOT_HELD_ELSEWHERE), kept("d", KEPT_CHANGED)], vec![kept("e", "Resource busy")]),
            true,
        )
        .unwrap();
    assert_eq!(many.title, format!("5 files stay on this {}", trs_platform::computer!()));
    let pc = trs_platform::computer!();
    assert_eq!(
        many.body,
        format!(
            "2 are set to always stay on this {pc}: stop keeping them here first. 1 is kept until a device that has it too is connected, so nothing is lost. 1 changed on this {pc} and is synced first. 1 couldn't be freed: it may be open in another app. (“{}” in Folder Sync)",
            f.name
        )
    );
    // All for one reason: "They".
    let same = n.sync_evict_notice(f.id.clone(), evicted(vec![kept("a", NOT_HELD_ELSEWHERE), kept("b", NOT_HELD_ELSEWHERE)], Vec::new()), true).unwrap();
    assert!(same.body.starts_with("They are kept until a device that has them too is connected"), "{}", same.body);
    // A reason the notice has no words of its own for is passed on.
    let odd = evict_notice(&f.id, Some("Photos"), &evicted(vec![kept("x", "some new reason")], Vec::new()), true).unwrap();
    assert_eq!(odd.body, "It: some new reason. (“Photos” in Folder Sync)");

    // One notice per folder: a later free replaces the earlier.
    assert_eq!(one.key, many.key);
    assert!(one.key.as_ref().is_some_and(|k| k.len() <= 64));
    let elsewhere = evict_notice("another", Some("Docs"), &evicted(vec![kept("x", KEPT_PINNED)], Vec::new()), true).unwrap();
    assert_ne!(elsewhere.key, one.key);
}
