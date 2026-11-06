use std::sync::Arc;

use super::*;
use crate::chat_files::ChatFileCard;
use crate::chat_markdown::{ChatRun, ChatTableCell};
use crate::chat_text::tests::{channel, topic};
use crate::host::host_windows_all;
use crate::library::tests::{mock, Clock};
use crate::mail::chat_tag_note;
use crate::sync::SyncGoneDevice;

/// 2027-01-15 08:00 UTC; the test clock is an hour east, so its day starts at 23:00 UTC the evening before.
const NOW: u64 = 1_800_000_000;
const DAY: u64 = 86_400;

/// What the Windows app has today.
const WINDOWS: [HostWindow; 4] = [HostWindow::GetInfo, HostWindow::MoveTo, HostWindow::NewChannel, HostWindow::ChatDialogs];

fn msg(id: &str, device: &str, at: u64, body: &str) -> SyncMessage {
    SyncMessage {
        id: id.into(),
        channel_id: "C-Team".into(),
        author_device: device.into(),
        author_name: format!("{device}'s Mac"),
        author_user: String::new(),
        is_me: false,
        agent: None,
        at_s: at,
        reply_to: None,
        topic: None,
        kind: SyncMessageKind::Text,
        body: body.into(),
        images: Vec::new(),
        files: Vec::new(),
        place: "C-Team".into(),
    }
}

fn member(id: &str, name: &str, role: SyncRole, is_me: bool, online: bool) -> SyncMember {
    SyncMember { device_id: id.into(), name: name.into(), user: String::new(), role, online, is_me, expires_s: None, removal: None, removal_waiting: false }
}

fn input(c: SyncChannel, messages: Vec<SyncMessage>) -> ChatPageInput {
    ChatPageInput {
        place: LibraryPlace::Chat { channel_id: c.id.clone(), topic_id: None },
        title: c.name.clone(),
        channel: Some(c),
        topic: None,
        direct: None,
        read: ChatPageRead { messages, pending: Vec::new(), follow: None, direct_agents: SyncAgentAccess::Off, own_devices: vec!["D0".into()], joined: Vec::new() },
        members: Vec::new(),
        now_unix_s: NOW,
    }
}

fn direct(online: Option<bool>, messages: Vec<SyncMessage>) -> ChatPageInput {
    ChatPageInput {
        place: LibraryPlace::Direct { device_id: "D2".into() },
        title: "Kari's iPhone".into(),
        channel: None,
        topic: None,
        direct: online.map(|online| SyncDirect { device_id: "D2".into(), name: "Kari's iPhone".into(), user: String::new(), online, unread: 0, last_message_s: None }),
        read: ChatPageRead { messages, pending: Vec::new(), follow: None, direct_agents: SyncAgentAccess::Read, own_devices: Vec::new(), joined: Vec::new() },
        members: Vec::new(),
        now_unix_s: NOW,
    }
}

fn page(i: ChatPageInput, host: &[HostWindow]) -> ChatPageModel {
    chat_page_model(i, Arc::new(Clock), host.to_vec()).unwrap()
}

/// The titles of a menu's entries, a submenu as `title[…]`, a separator as `-`, a disabled item in parentheses and a
/// checked one with ✓.
fn titles(entries: &[ChatPageMenuEntry]) -> Vec<String> {
    entries
        .iter()
        .map(|e| match e {
            ChatPageMenuEntry::Item { title, checked, enabled, .. } => {
                let t = if *checked { format!("✓{title}") } else { title.clone() };
                if *enabled { t } else { format!("({t})") }
            }
            ChatPageMenuEntry::Submenu { title, entries } => format!("{title}[{}]", titles(entries).join(", ")),
            ChatPageMenuEntry::Note { text } => format!("note:{text}"),
            ChatPageMenuEntry::Separator => "-".into(),
        })
        .collect()
}

fn find<'a>(entries: &'a [ChatPageMenuEntry], name: &str) -> &'a ChatPageMenuEntry {
    entries.iter().find(|e| matches!(e, ChatPageMenuEntry::Item { title, .. } if title == name)).unwrap_or_else(|| panic!("no {name}"))
}

fn text(blocks: &[ChatBlock]) -> String {
    blocks
        .iter()
        .map(|b| match b {
            ChatBlock::Paragraph { runs } | ChatBlock::Heading { runs, .. } | ChatBlock::Quote { runs } | ChatBlock::Item { runs, .. } => {
                crate::chat_markdown::runs_text(runs)
            }
            ChatBlock::CodeBlock { text, .. } => text.clone(),
            ChatBlock::Table { header, .. } => header.iter().map(|c: &ChatTableCell| crate::chat_markdown::runs_text(&c.runs)).collect::<Vec<_>>().join("|"),
            ChatBlock::Rule => "---".into(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn a_run_of_one_author_shows_it_once() {
    let a = msg("m1aaaaaaaa", "D1", NOW - 600, "one");
    let mut agent = msg("m4aaaaaaaa", "D1", NOW - 280, "bot");
    agent.agent = Some("ci".into());
    let mut reply = msg("m5aaaaaaaa", "D1", NOW - 270, "re");
    reply.reply_to = Some(a.id.clone());
    let ms = vec![
        a.clone(),
        msg("m2aaaaaaaa", "D1", NOW - 500, "two: same author within five minutes"),
        msg("m3aaaaaaaa", "D2", NOW - 490, "another device"),
        agent,
        reply,
        msg("m6aaaaaaaa", "D1", NOW - 270 + RUN_S + 1, "later than five minutes"),
    ];
    let p = page(input(channel("Team", 3, 2, 0, false), ms), &host_windows_all());
    let starts: Vec<bool> = p.messages.iter().map(|c| c.starts_run).collect();
    assert_eq!(starts, [true, false, true, true, true, true], "an agent's message and a reply start their own run");
    let lines: Vec<bool> = p.messages.iter().map(|c| c.meta.is_some()).collect();
    assert_eq!(lines, [false, true, true, true, true, true], "who wrote a run is said once, under its last message");
    let q = p.messages[4].reply_quote.as_ref().unwrap();
    assert_eq!((q.title.as_str(), q.text.as_str()), ("D1's Mac", "one"));
    assert!(!starts_run(&msg("x", "D1", 1_300, ""), Some(&msg("y", "D1", 1_000, ""))), "exactly five minutes is the same run");
    assert_eq!(p.messages[3].who, ChatAuthorKind::TheirAgent);
    // Each person's colour is stable; their agent's is theirs; the quote carries the quoted message's.
    let colours: Vec<Option<u32>> = p.messages.iter().map(|c| c.palette).collect();
    assert!(colours.iter().all(|c| c.is_some_and(|i| i < CHAT_PALETTE_SIZE)));
    assert_eq!(colours[0], colours[3], "D1's agent takes D1's colour");
    let none = HashMap::new();
    assert_eq!(colours[0], palette(&msg("x", "D1", 0, ""), ChatAuthorKind::Theirs, &none), "the same for every message of D1");
    let q = p.messages[4].reply_quote.as_ref().unwrap();
    assert_eq!((q.message_id.as_str(), q.who, q.palette), ("m1aaaaaaaa", ChatAuthorKind::Theirs, colours[0]));
    assert_eq!(palette(&msg("x", "D0", 0, ""), ChatAuthorKind::Mine, &none), None, "the user's: the accent");
    assert_eq!((chat_palette_size(), chat_code_copy_help().as_str()), (8, "Copy"));
    assert_eq!(p.messages[1].time, format!("T{}", NOW - 500), "every message has its time for the pointer");
}

#[test]
fn times_say_today_yesterday_or_the_day_across_day_breaks() {
    let clock = Clock;
    // The test clock's day starts at 23:00 UTC (an hour east).
    let midnight = (NOW + 3_600) / DAY * DAY - 3_600;
    assert_eq!(message_time(&clock, NOW - 60, NOW), format!("T{}", NOW - 60));
    assert_eq!(message_time(&clock, midnight, NOW), format!("T{midnight}"), "the first second of today");
    assert_eq!(message_time(&clock, midnight - 1, NOW), format!("Yesterday T{}", midnight - 1));
    assert_eq!(message_time(&clock, midnight - DAY, NOW), format!("Yesterday T{}", midnight - DAY));
    assert_eq!(message_time(&clock, midnight - DAY - 1, NOW), format!("D{0} T{0}", midnight - DAY - 1));
    assert_eq!(message_time(&clock, NOW - 400 * DAY, NOW), format!("D{0}Y T{0}", NOW - 400 * DAY), "another year names it");
    assert_eq!(message_time(&clock, NOW + 2 * DAY, NOW), format!("D{0} T{0}", NOW + 2 * DAY), "one dated ahead says its day");
    // A run goes on across midnight: its line is under its last message, with that message's time.
    let p = page(input(channel("Team", 2, 1, 0, false), vec![msg("m1aaaaaaaa", "D1", midnight - 60, "late"), msg("m2aaaaaaaa", "D1", midnight + 60, "early")]), &[]);
    assert!(p.messages[0].meta.is_none());
    assert_eq!(p.messages[1].meta.as_ref().unwrap().detail, format!("T{}", midnight + 60));
    assert_eq!(p.messages[0].time, format!("Yesterday T{}", midnight - 60));
}

#[test]
fn authors_are_people_devices_you_or_agents() {
    let mut m = msg("m1aaaaaaaa", "D1", NOW, "hi");
    let own: Vec<String> = vec!["D0".into(), "D3".into()];
    let a = |m: &SyncMessage| message_meta(m, author_kind(m, &own), &Clock, NOW);
    let line = |x: ChatMessageMeta| (x.name, x.detail, x.initial, x.help);
    let t = format!("T{NOW}");
    // A device that is its own name says it once.
    assert_eq!(line(a(&m)), ("D1's Mac".into(), t.clone(), "D".into(), String::new()));
    m.author_user = "Kari".into();
    assert_eq!(line(a(&m)), ("Kari".into(), format!("D1's Mac · {t}"), "K".into(), "Kari on D1's Mac".into()));
    assert_eq!(author(&m), "Kari (D1's Mac)", "notifications keep the device");
    m.is_me = true;
    assert_eq!(line(a(&m)), ("You".into(), t.clone(), "Y".into(), String::new()), "this device: no device");
    assert_eq!(author(&m), "You");
    m.agent = Some("Seedwell".into());
    assert_eq!(line(a(&m)), ("Seedwell".into(), format!("D1's Mac · {t}"), "S".into(), "Seedwell, an agent on D1's Mac".into()));
    assert_eq!(author(&m), "Seedwell on D1's Mac", "an agent on this computer names the computer too");
    m.author_name.clear();
    assert_eq!(chat_message_author(m.clone()), "Seedwell on Unnamed device");
    // Whose: this device and the user's other devices are the user's, with their agents; the rest are others'.
    let kind = |device: &str, me: bool, agent: Option<&str>| {
        let mut x = msg("m9aaaaaaaa", device, NOW, "x");
        x.is_me = me;
        x.agent = agent.map(str::to_owned);
        author_kind(&x, &own)
    };
    assert_eq!(kind("D0", true, None), ChatAuthorKind::Mine);
    assert_eq!(kind("D3", false, None), ChatAuthorKind::Mine, "another of the user's devices");
    assert_eq!(kind("D0", true, Some("ci")), ChatAuthorKind::OurAgent);
    assert_eq!(kind("D3", false, Some("ci")), ChatAuthorKind::OurAgent);
    assert_eq!(kind("D1", false, None), ChatAuthorKind::Theirs);
    assert_eq!(kind("D1", false, Some("ci")), ChatAuthorKind::TheirAgent);
    assert_eq!(author_kind(&msg("x", "D0", NOW, ""), &[]), ChatAuthorKind::Theirs, "in no group: only this device is the user's");
    let mut phone = msg("m2aaaaaaaa", "D3", NOW, "from my phone");
    phone.author_name = "Kari's iPhone".into();
    let x = a(&phone);
    assert_eq!((x.name.as_str(), x.detail.as_str(), x.help.as_str()), ("You", format!("Kari's iPhone · {t}").as_str(), "You on Kari's iPhone"));
    let mut mine = msg("m3aaaaaaaa", "D0", NOW, "mine");
    mine.is_me = true;
    let p = page(input(channel("Team", 2, 1, 0, false), vec![m, mine]), &[]);
    assert_eq!(p.messages.iter().map(|c| c.who).collect::<Vec<_>>(), [ChatAuthorKind::OurAgent, ChatAuthorKind::Mine]);
    assert!(!p.bubbles, "a channel is one column");
}

#[test]
fn previews_and_banners_are_one_short_line() {
    let m = |b: &str| msg("m1aaaaaaaa", "D1", NOW, b);
    assert_eq!(preview(&m("line one\nline two"), 120), "line one line two");
    assert_eq!(preview(&m("## Plan\n\n1. **Build** it\n- `ship`"), 120), "Plan 1. Build it • ship", "without its Markdown");
    assert_eq!(preview(&m(&"x".repeat(300)), 10), "xxxxxxxxx…");
    assert_eq!(preview(&m(&"ø".repeat(10)), 10), "øøøøøøøøøø", "characters, not bytes");
    assert_eq!(preview(&SyncMessage { kind: SyncMessageKind::Json, ..m("{}") }, 120), "JSON message");
    assert_eq!(preview(&SyncMessage { kind: SyncMessageKind::Other, ..m("?") }, 120), "A message this version cannot show");
    assert_eq!(preview(&m(samples::BUG_MAIL), 120), "Bug: The open-source graph serves the wrong repo after a switch");
    assert!(preview(&m(samples::STATUS_MAIL), 120).starts_with("Done: Fixed in 0.17.13"));
    let mut agent = m("Build is ready");
    agent.agent = Some("ci".into());
    let n = chat_notification_text(chat_place_title(channel("Team", 2, 1, 0, false), None), agent);
    assert_eq!((n.title.as_str(), n.body.as_str()), ("Team", "ci on D1's Mac: Build is ready"));
    assert_eq!(chat_place_title(channel("Team", 2, 1, 0, false), Some(topic("Dinner", "C-Team", 0))), "Team › Dinner");
    // A message of only attachments says what they are.
    let file = ChatFileCard {
        hash: "h".into(),
        name: "Plan.pdf".into(),
        size_text: "1.2 MB".into(),
        icon: crate::chat_files::ChatFileIcon::Document,
        state: crate::chat_files::ChatFileState::Available,
        status: String::new(),
        progress: None,
        path: None,
        actions: Vec::new(),
    };
    assert_eq!(preview(&SyncMessage { files: vec![file.clone()], ..m("") }, 120), "Plan.pdf · 1.2 MB");
    // A card refers to the image and file models as they are.
    let p = page(input(channel("Team", 2, 1, 0, false), vec![SyncMessage { files: vec![file.clone()], ..m("") }]), &[]);
    assert_eq!(p.messages[0].files, vec![file]);
    assert_eq!(p.messages[0].place, "C-Team");
    assert_eq!(p.messages[0].body, ChatMessageBody::Markdown { blocks: Vec::new() });
}

#[test]
fn mails_have_a_banner_and_tags_are_lines_by_owner() {
    let ms = chat_sample_messages();
    let p = page(input(channel("Bridge", 2, 2, 0, false), ms.clone()), &host_windows_all());
    assert_eq!(p.messages.len(), 8, "the two tag notes are lines, not cards");
    let ChatMessageBody::Mail { banner, blocks } = &p.messages[6].body else { panic!("{:?}", p.messages[6].body) };
    assert_eq!(
        banner,
        &ChatMailBanner::Mail {
            chip: Some("Bug".into()),
            tint: ChatMailTint::Red,
            to: Some("to kglite".into()),
            re: None,
            summary: Some("The open-source graph serves the wrong repo after a switch".into())
        }
    );
    assert!(text(blocks).starts_with("After repo_management('pydata/xarray'), graph_overview still answers"), "the header is not text");
    assert_eq!(
        p.messages[6].tags,
        [ChatTagLine { text: "kglite: Seen, Backlog".into(), seen: false }, ChatTagLine { text: "You: Needs triage".into(), seen: false }]
    );
    let ChatMessageBody::Mail { banner, .. } = &p.messages[7].body else { panic!() };
    assert_eq!(banner, &ChatMailBanner::Status { title: "Done".into(), done: true });
    // A seen receipt alone is an eye; a progress status a clock.
    let mut receipt = msg("r1aaaaaaaa", "D3", NOW, &chat_tag_note(&ms[0].id, vec!["seen".into()]));
    receipt.agent = Some("kglite".into());
    let open = msg("s1aaaaaaaa", "D3", NOW, "---\nmail: 1\ntype: status\nstate: open\n---\nHalf way");
    let hook = msg("h1aaaaaaaa", "D3", NOW, "---\nmail: 1\ntype: hook\nsummary: Hook set\n---\n");
    let p = page(input(channel("Bridge", 2, 2, 0, false), vec![ms[0].clone(), receipt, open, hook]), &[]);
    assert_eq!(p.messages[0].tags, [ChatTagLine { text: "kglite: Seen".into(), seen: true }]);
    assert!(matches!(&p.messages[1].body, ChatMessageBody::Mail { banner: ChatMailBanner::Status { title, done: false }, .. } if title == "Progress"));
    assert!(matches!(&p.messages[2].body, ChatMessageBody::Mail { banner: ChatMailBanner::Note { line, seen: false }, blocks } if line == "Hook set" && blocks.is_empty()));
    // Your tags: switching one on or off sends your whole new set; Clear My Tags only while you have some.
    let m = &chat_page_model(input(channel("Bridge", 2, 2, 0, false), ms.clone()), Arc::new(Clock), Vec::new()).unwrap().messages[6];
    let menu = m.tag_menu.as_ref().unwrap();
    assert_eq!(titles(&menu.entries), ["Seen", "Actioned", "Backlog", "✓Needs triage", "-", "Clear My Tags"]);
    let ChatPageMenuEntry::Item { command, toggle: true, .. } = find(&menu.entries, "Seen") else { panic!() };
    assert_eq!(command, &ChatPageCommand::SetTags { message_id: m.id.clone(), tags: vec!["needs-triage".into(), "seen".into()] });
    let ChatPageMenuEntry::Item { command, .. } = find(&menu.entries, "Needs triage") else { panic!() };
    assert_eq!(command, &ChatPageCommand::SetTags { message_id: m.id.clone(), tags: Vec::new() });
    assert_eq!(titles(&p.messages[1].tag_menu.as_ref().unwrap().entries), ["Seen", "Actioned", "Backlog", "Needs triage"]);
}

#[test]
fn json_and_unknown_messages_and_nasty_text() {
    let json = SyncMessage { kind: SyncMessageKind::Json, ..msg("j1aaaaaaaa", "D1", NOW, r#"{"a":1}"#) };
    let other = SyncMessage { kind: SyncMessageKind::Other, ..msg("o1aaaaaaaa", "D2", NOW, "\u{0}binary") };
    let nasty = msg("n1aaaaaaaa", "D3", NOW, &format!("**open [x](<y>\n```\n{}", "`".repeat(5_000)));
    let p = page(input(channel("Team", 2, 1, 0, false), vec![json, other, nasty]), &[]);
    assert_eq!(p.messages[0].body, ChatMessageBody::Json { text: r#"{"a":1}"#.into() });
    assert_eq!(p.messages[1].body, ChatMessageBody::Unsupported { text: "A message this version cannot show".into() });
    let ChatMessageBody::Markdown { blocks } = &p.messages[2].body else { panic!() };
    assert_eq!(blocks.len(), 2);
    assert!(matches!(&blocks[0], ChatBlock::Paragraph { runs } if runs == &[ChatRun { text: "**open [x](<y>".into(), bold: false, italic: false, strikethrough: false, code: false, link: None, image: None }]));
}

#[test]
fn the_channel_menu_with_every_window_and_with_the_windows_set() {
    let mut c = channel("Bridge", 3, 2, 0, false);
    c.agents = true;
    let follow = crate::devices_text::follow_line_new(&["MacBook".to_string()], false);
    let mut i = input(c.clone(), Vec::new());
    i.read.follow = Some(follow.clone());
    let p = page(i.clone(), &host_windows_all());
    let t = &p.toolbar;
    assert_eq!(t.tag.as_ref().map(|x| x.text.as_str()), Some("agents"));
    assert_eq!(t.members.as_ref().map(|b| (b.title.as_str(), b.help.as_str())), Some(("Members", "Members")));
    assert_eq!(t.share.as_ref().map(|b| b.title.as_str()), Some("Share…"));
    assert_eq!(
        titles(&t.menu.entries),
        [
            "Mute".to_string(),
            "Agents: Yes[(✓Yes), No, -, note:Agents on every member's device can read and post here. Their messages are marked.]".into(),
            follow.menu_title.clone(),
            "Wake Command…".into(),
            "-".into(),
            "New Topic…".into(),
            "Rename…".into(),
            "-".into(),
            "Leave Channel…".into()
        ]
    );
    let ChatPageMenuEntry::Item { dialog: Some(d), .. } = find(&t.menu.entries, "Leave Channel…") else { panic!("Leave asks first") };
    assert_eq!(d, &chat_leave_dialog(c.clone()));
    let ChatPageMenuEntry::Item { command, toggle, help, .. } = find(&t.menu.entries, &follow.menu_title) else { panic!() };
    assert_eq!((command, *toggle, help.as_str()), (&ChatPageCommand::SetFollow { on: true }, true, follow.line.as_str()));
    let ChatPageMenuEntry::Item { dialog, .. } = find(&t.menu.entries, "Mute") else { panic!() };
    assert!(dialog.is_none(), "Mute needs no question");
    // The Windows app (no Share dialog yet): Share… goes; the dialogs stay. Without the dialogs, their items go too.
    let w = page(i.clone(), &WINDOWS);
    assert!(w.toolbar.share.is_none());
    assert_eq!(titles(&w.toolbar.menu.entries), titles(&t.menu.entries));
    let bare = page(i, &[]);
    assert_eq!(&titles(&bare.toolbar.menu.entries)[2..], std::slice::from_ref(&follow.menu_title), "no separator left at the end");
    // A reader: no Share…, New Topic… disabled, Agents shown but not changeable, the wake command off while closed.
    let mut r = channel("Family", 2, 1, 0, true);
    r.role = SyncRole::ReadOnly;
    let p = page(input(r, Vec::new()), &host_windows_all());
    assert!(p.toolbar.share.is_none() && p.toolbar.tag.is_none());
    assert_eq!(
        titles(&p.toolbar.menu.entries),
        [
            "Unmute",
            "Agents: No[(Yes), (✓No), -, note:Only the channel's owners and admins can change this.]",
            "(Wake Command…)",
            "-",
            "(New Topic…)",
            "Rename…",
            "-",
            "Leave Channel…"
        ]
    );
    assert_eq!(p.composer.cannot_post.as_deref(), Some("Read only"));
    assert!(!p.composer.can_post && !p.composer.attach);
}

/// An admin's removal of a channel member waits for the owner's device (SPEC §7G.1), or came back refused: the member's
/// row in the panel says so, as the folder page's row does. Fails when the panel shows only a disabled Remove from
/// Channel with no line saying why (the gap the Windows agent found), or drops the owner's reason for refusing.
#[test]
fn a_members_pending_or_refused_removal_is_its_rows_line() {
    use trs_sync_net::removals::Asked;
    let mut c = channel("Team", 3, 1, 0, false);
    c.role = SyncRole::Admin;
    let mut i = input(c, Vec::new());
    let waiting = SyncMember { removal: Some(crate::sync_text::removal_line(&Asked::Waiting)), removal_waiting: true, ..member("D2", "Studio iMac", SyncRole::ReadWrite, false, false) };
    let why = Asked::Refused("the owner decided about this device since it was asked".into());
    let refused = SyncMember { removal: Some(crate::sync_text::removal_line(&why)), ..member("D3", "Air", SyncRole::ReadOnly, false, false) };
    i.members = vec![member("D0", "MacBook", SyncRole::Admin, true, false), waiting, refused, member("D4", "Mini", SyncRole::ReadWrite, false, true)];
    let rows = page(i, &[]).members.unwrap().rows;
    let row = |id: &str| rows.iter().find(|r| r.device_id == id).unwrap();
    assert_eq!(row("D2").removal.as_deref(), Some("Removal asked: waiting for the owner"));
    let ChatPageMenuEntry::Item { enabled, .. } = find(&row("D2").role_menu.as_ref().unwrap().entries, "Remove from Channel") else { panic!() };
    assert!(!enabled, "not asked twice");
    let line = row("D3").removal.clone().unwrap_or_default();
    assert!(line.contains("The owner decided about this device since it was asked"), "the refusal's reason: {line:?}");
    assert_eq!((row("D0").removal.as_ref(), row("D4").removal.as_ref()), (None, None), "nothing asked: no line");
}

#[test]
fn members_roles_and_gone_devices() {
    let mut c = channel("Team", 3, 2, 0, false);
    c.gone = vec![SyncGoneDevice { device_id: "G1".into(), name: "Old iPad".into(), last_seen_s: None }];
    let mut i = input(c.clone(), Vec::new());
    i.members = vec![
        member("D2", "Studio iMac", SyncRole::ReadWrite, false, false),
        member("D0", "MacBook", SyncRole::Owner, true, false),
        member("D1", "Air", SyncRole::Admin, false, true),
    ];
    let panel = page(i.clone(), &host_windows_all()).members.unwrap();
    assert_eq!(panel.title, "Members");
    let rows: Vec<(&str, bool, &str)> = panel.rows.iter().map(|r| (r.name.as_str(), r.online, r.role.as_str())).collect();
    assert_eq!(rows, [(concat!("MacBook (this ", computer!(), ")"), true, "Owner"), ("Air", true, "Admin"), ("Studio iMac", false, "Read & write")]);
    assert!(panel.rows[0].role_menu.is_none() && panel.rows[0].message.is_none(), "not for this computer");
    let menu = panel.rows[2].role_menu.as_ref().unwrap();
    assert_eq!(menu.title, "Read & write");
    assert_eq!(titles(&menu.entries), ["Owner", "Admin", "(Read & write)", "Read only", "-", "Remove from Channel"]);
    let ChatPageMenuEntry::Item { command, dialog: None, .. } = find(&menu.entries, "Remove from Channel") else { panic!() };
    assert_eq!(command, &ChatPageCommand::RemoveDevice { device_id: "D2".into() });
    let m = panel.rows[2].message.as_ref().unwrap();
    assert_eq!((m.help.as_str(), m.accessibility_label.as_deref()), ("Message Studio iMac privately", Some("Message Privately")));
    assert_eq!(m.command, ChatPageCommand::MessagePrivately { device_id: "D2".into(), name: "Studio iMac".into() });
    let g = &panel.gone[0];
    assert_eq!(g.line, concat!("Old iPad has never connected to this ", computer!()));
    let q = g.button.question.as_ref().unwrap();
    assert_eq!((g.button.title.as_str(), q.title.as_str(), q.destructive), ("Remove from Channel…", "Remove Old iPad from “Team”?", true));
    // An admin gets Remove for a user (Studio iMac) only; a user gets no menu.
    let mut admin = c.clone();
    admin.role = SyncRole::Admin;
    i.channel = Some(admin);
    let rows = page(i.clone(), &[]).members.unwrap().rows;
    assert!(rows[0].role_menu.is_none() && rows[1].role_menu.is_none(), "an admin changes an owner or an admin");
    assert_eq!(titles(&rows[2].role_menu.as_ref().unwrap().entries), ["Remove from Channel"]);
    let mut user = c;
    user.role = SyncRole::ReadWrite;
    i.channel = Some(user);
    assert!(page(i, &[]).members.unwrap().rows.iter().all(|r| r.role_menu.is_none()));
}

#[test]
fn the_composer_says_when_it_may_post_and_send() {
    let p = page(input(channel("Family", 2, 1, 0, false), Vec::new()), &host_windows_all());
    let c = &p.composer;
    assert!(c.can_post && c.attach && c.cannot_post.is_none());
    assert_eq!((c.placeholder.as_str(), c.send_title.as_str(), c.reply_cancel_help.as_str()), ("Message Family", "Send", "Cancel Reply (Esc)"));
    assert!(c.send_help.starts_with("Send (") && c.send_help.contains(trs_platform::return_key!()));
    assert_eq!(p.empty.as_deref(), Some("No messages yet"));
    // A topic's page is named by the topic.
    let mut i = input(channel("Family", 2, 1, 0, false), Vec::new());
    i.place = LibraryPlace::Chat { channel_id: "Family".into(), topic_id: Some("Dinner".into()) };
    i.topic = Some(topic("Dinner", "Family", 0));
    let t = page(i, &[]);
    assert_eq!((t.composer.placeholder.as_str(), t.topic.as_deref(), t.place.as_str()), ("Message Dinner", Some("Dinner"), "Family"));
    // Send: text that is not only spaces, or an attachment.
    assert!(!chat_composer_can_send(" \n\t ".into(), 0));
    assert!(chat_composer_can_send(" \n ".into(), 1));
    assert!(chat_composer_can_send("hi".into(), 0));
    assert_eq!(chat_composer_body("  hi there \n".into()).as_deref(), Some("hi there"));
    assert_eq!(chat_composer_body(" \n".into()), None);
    assert_eq!(chat_composer_key(false, false), ChatComposerKey::Send);
    assert_eq!(chat_composer_key(true, false), ChatComposerKey::NewLine);
    assert_eq!(chat_composer_key(false, true), ChatComposerKey::NewLine);
    // Replying: the card's line for the bar; reply and tags only where it may post.
    let p = page(input(channel("Family", 2, 1, 0, false), vec![msg("m1aaaaaaaa", "D1", NOW, &format!("**Long** {}", "word ".repeat(30)))]), &[]);
    let card = &p.messages[0];
    assert_eq!(card.replying.title, "Replying to D1's Mac");
    assert!(card.replying.text.starts_with("Long word") && card.replying.text.ends_with('…') && card.replying.text.chars().count() == 120);
    assert_eq!(card.reply.as_ref().map(|b| &b.command), Some(&ChatPageCommand::Reply { message_id: "m1aaaaaaaa".into() }));
    let mut reader = channel("Family", 2, 1, 0, false);
    reader.role = SyncRole::ReadOnly;
    let p = page(input(reader, vec![msg("m1aaaaaaaa", "D1", NOW, "x")]), &[]);
    assert!(p.messages[0].reply.is_none() && p.messages[0].tag_menu.is_none());
}

#[test]
fn private_conversations_differ() {
    let p = page(direct(Some(true), Vec::new()), &host_windows_all());
    assert_eq!(p.place, "dm:D2");
    assert!(p.toolbar.tag.is_none() && p.toolbar.members.is_none() && p.toolbar.share.is_none() && p.members.is_none());
    assert_eq!(
        titles(&p.toolbar.menu.entries),
        [concat!("Agents on This ", computer!(), "[Off, ✓Read, Read and post]"), "-", "Delete Conversation…"]
    );
    let ChatPageMenuEntry::Item { dialog: Some(d), command, .. } = find(&p.toolbar.menu.entries, "Delete Conversation…") else { panic!() };
    assert_eq!((d, command), (&chat_delete_conversation_dialog("Kari's iPhone".into()), &ChatPageCommand::DeleteConversation));
    assert_eq!(p.empty.as_deref(), Some("No messages yet"));
    assert!(p.bubbles, "a private conversation is drawn as bubbles");
    assert!(p.composer.can_post);
    assert_eq!(p.composer.placeholder, "Message Kari's iPhone");
    // Not connected directly: no field, the core's sentence instead.
    let off = page(direct(Some(false), vec![msg("m1aaaaaaaa", "D2", NOW, "hei")]), &WINDOWS);
    assert!(!off.composer.can_post);
    assert_eq!(off.composer.cannot_post.as_deref(), Some("You can write when Kari's iPhone is connected directly"));
    assert!(off.messages[0].reply.is_none());
    // Just started from a channel's members (not listed yet): it may write.
    assert!(page(direct(None, Vec::new()), &[]).composer.can_post);
    // Without the dialogs: no Delete Conversation…, and no separator left.
    assert_eq!(titles(&page(direct(Some(true), Vec::new()), &[]).toolbar.menu.entries).len(), 1);
}

#[test]
fn a_page_needs_its_conversation_and_changes_its_signature_with_it() {
    let mut i = input(channel("Team", 2, 1, 0, false), Vec::new());
    let a = page(i.clone(), &[]);
    assert_eq!(a.signature, page(i.clone(), &[]).signature);
    i.read.messages.push(msg("m1aaaaaaaa", "D1", NOW, "hi"));
    assert_ne!(a.signature, page(i.clone(), &[]).signature);
    i.channel = None;
    assert!(chat_page_model(i.clone(), Arc::new(Clock), Vec::new()).is_none(), "a channel that is gone");
    i.place = LibraryPlace::Downloads;
    assert!(chat_page_model(i, Arc::new(Clock), Vec::new()).is_none());
}

#[test]
fn reading_a_page_marks_it_read() {
    let (core, _dir) = mock();
    let family = core.chat_channels().into_iter().find(|c| c.name == "Family").unwrap();
    assert!(family.unread > 0, "the mock starts with unread messages");
    let read = core.chat_page_read(LibraryPlace::Chat { channel_id: family.id.clone(), topic_id: None });
    assert!(!read.messages.is_empty() && read.follow.is_some());
    assert_eq!(read.own_devices.len(), 2, "the user's devices come with the read (the mock: this Mac and a phone)");
    let after = core.chat_channels().into_iter().find(|c| c.name == "Family").unwrap();
    assert_eq!(general_unread(&after, &core.chat_topics(family.id.clone())), 0, "General is read");
    assert!(core.chat_page_read(LibraryPlace::Chat { channel_id: "gone".into(), topic_id: None }).messages.is_empty());
    assert!(core.chat_page_read(LibraryPlace::Downloads).messages.is_empty());
}

#[test]
fn the_sample_conversation_renders_as_before() {
    let ms = chat_sample_messages();
    let p = page(input(channel("C", 2, 1, 0, false), ms), &[]);
    assert!(p.messages.iter().all(|c| c.starts_run && c.meta.is_some()), "every sample is by another device");
    assert_eq!(p.messages[2].meta.as_ref().unwrap().name, "Kari's iPhone");
    assert!(p.messages.iter().all(|c| !c.gallery), "no images, no gallery");
    // One image is shown alone; more are a gallery, in their order.
    let shots: Vec<_> = crate::chat_images::chat_image_layout_samples(64).into_iter().map(|s| s.image).collect();
    let with = |n: usize| SyncMessage { images: shots[..n].to_vec(), ..msg(&format!("i{n}aaaaaaaa"), "D1", NOW, "") };
    let p2 = page(input(channel("C", 2, 1, 0, false), vec![with(1), with(5)]), &[]);
    assert_eq!(p2.messages.iter().map(|c| c.gallery).collect::<Vec<_>>(), [false, true]);
    // A reference to an image the message carries is a chip; one to an image it does not carry is text.
    let refs = SyncMessage { body: "See ![Image 2](att:2), not ![Image 7](att:7)".into(), ..with(5) };
    let p3 = page(input(channel("C", 2, 1, 0, false), vec![refs]), &[]);
    let ChatMessageBody::Markdown { blocks } = &p3.messages[0].body else { panic!() };
    let ChatBlock::Paragraph { runs } = &blocks[0] else { panic!() };
    assert_eq!(runs.iter().map(|r| (r.text.as_str(), r.image)).collect::<Vec<_>>(), [("See ", None), ("Image 2", Some(1)), (", not Image 7", None)]);
    assert_eq!(p2.messages[1].images.iter().map(|i| i.hash.as_str()).collect::<Vec<_>>(), shots[..5].iter().map(|i| i.hash.as_str()).collect::<Vec<_>>());
    let ChatMessageBody::Markdown { blocks } = &p.messages[1].body else { panic!() };
    assert!(matches!(&blocks[1], ChatBlock::Table { header, rows } if header.len() == 4 && rows.len() == 3));
}

fn joined(list: &[(&str, u64, &str)]) -> Vec<SyncJoined> {
    list.iter().map(|&(d, at_s, user)| SyncJoined { device_id: d.into(), at_s, user: user.into() }).collect()
}

#[test]
fn people_take_a_channels_colours_in_the_order_they_joined_and_keep_them() {
    // Join order, not the order listed; a user's devices are one person.
    let j = joined(&[("C", 30, "Ola"), ("A", 10, "Kari"), ("B", 20, ""), ("A2", 40, "Kari"), ("D", 50, "Per")]);
    let map = palette_map(&j);
    let of = |d: &str| map[d];
    assert_eq!((of("A"), of("B"), of("C"), of("A2"), of("D")), (0, 1, 2, 0, 3));
    // Stable: the same membership, in any order, on any device.
    let mut shuffled = j.clone();
    shuffled.reverse();
    assert_eq!(palette_map(&shuffled), map);
    // One who left keeps their place (the membership keeps them): nobody's colour changes; a later joiner takes the next.
    let later = joined(&[("C", 30, "Ola"), ("A", 10, "Kari"), ("B", 20, ""), ("A2", 40, "Kari"), ("D", 50, "Per"), ("E", 60, "")]);
    let m2 = palette_map(&later);
    assert_eq!((m2["A"], m2["B"], m2["C"], m2["D"], m2["E"]), (0, 1, 2, 3, 4));
    // Eight people never share a colour; the ninth takes the first again.
    let many: Vec<SyncJoined> = (0..10).map(|n| SyncJoined { device_id: format!("P{n}"), at_s: n, user: String::new() }).collect();
    let m3 = palette_map(&many);
    let first8: std::collections::HashSet<u32> = (0..8).map(|n| m3[&format!("P{n}")]).collect();
    assert_eq!(first8.len(), 8);
    assert_eq!((m3["P8"], m3["P9"]), (0, 1), "from the ninth person on, they repeat");
    // On the cards: an agent takes its person's colour; a quote the quoted person's.
    let mut c = channel("Team", 3, 2, 0, false);
    c.agents = true;
    let mut i = input(c, vec![msg("m1aaaaaaaa", "B", NOW - 60, "hi"), SyncMessage { agent: Some("bot".into()), ..msg("m2aaaaaaaa", "B", NOW - 50, "x") }]);
    i.read.messages.push(SyncMessage { reply_to: Some("m1aaaaaaaa".into()), ..msg("m3aaaaaaaa", "C", NOW - 40, "re") });
    i.read.joined = j.clone();
    let p = page(i, &[]);
    assert_eq!(p.messages.iter().map(|c| c.palette).collect::<Vec<_>>(), [Some(1), Some(1), Some(2)]);
    assert_eq!(p.messages[2].reply_quote.as_ref().and_then(|q| q.palette), Some(1));
    // A device the channel does not list (a private conversation): a colour from its id, the same everywhere.
    let none = HashMap::new();
    let d = palette(&msg("x", "ZZ", 0, ""), ChatAuthorKind::Theirs, &none);
    assert!(d.is_some_and(|i| i < CHAT_PALETTE_SIZE) && d == palette(&msg("y", "ZZ", 9, ""), ChatAuthorKind::TheirAgent, &none));
}
