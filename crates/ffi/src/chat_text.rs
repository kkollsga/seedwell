//! Channels' words (SPEC 7G.7) that the window's sidebar and title bar, the conversation pages and the notifications
//! share (docs/CROSS-PLATFORM.md §1): a device by its name and user, a channel's and a topic's grey lines, the unread
//! count of a channel's General, and what agents may do (a channel's Agents: Yes/No, private messages' switch).
//!
//! A date in the user's own style ("1 Nov") stays the host's: the functions that show one take it already written
//! (`…_day`), like the sync texts.

use trs_control::display as d;

use crate::sidebar::SidebarTag;
use crate::sync::{SyncAgentAccess, SyncChannel, SyncRole, SyncTopic};

/// A device that never said its name.
pub(crate) const UNNAMED_DEVICE: &str = "Unnamed device";

/// A device with its user, as every device names it (Settings › Folder Sync): "Kari (Studio iMac)", the device alone
/// when its user gave no name, "Unnamed device" for a device that never said its name.
pub(crate) fn named(device: &str, user: &str) -> String {
    let d = if device.is_empty() { UNNAMED_DEVICE } else { device };
    if user.is_empty() { d.to_owned() } else { format!("{user} ({d})") }
}

/// "Kari (Studio iMac)", "Studio iMac", "Unnamed device": a device with its user.
#[uniffi::export]
pub fn chat_named(device: &str, user: &str) -> String {
    named(device, user)
}

/// A channel's grey line in the list: "2 of 3 online", "Only this Mac · Muted"; while it joins, what it does
/// ("Looking for Anna's Mac mini on the local network…").
pub(crate) fn subtitle(channel: &SyncChannel) -> String {
    let mut parts = vec![if let Some(p) = &channel.progress {
        p.clone()
    } else if let Some(w) = &channel.waiting {
        // Nobody else connected: whom it waits for, from the core.
        w.clone()
    } else if channel.members > 1 {
        format!("{} of {} online", channel.online, channel.members)
    } else {
        concat!("Only this ", trs_platform::computer!()).to_owned()
    }];
    if channel.muted {
        parts.push("Muted".into());
    }
    parts.join(" · ")
}

/// A channel's grey line in the list: "2 of 3 online", "Only this Mac · Muted".
#[uniffi::export]
pub fn chat_subtitle(channel: SyncChannel) -> String {
    subtitle(&channel)
}

/// What agents on this computer may do with private messages: "Off", "Read", "Read and post".
#[uniffi::export]
pub fn chat_agents_label(access: SyncAgentAccess) -> String {
    match access {
        SyncAgentAccess::Off => "Off",
        SyncAgentAccess::Read => "Read",
        SyncAgentAccess::ReadPost => "Read and post",
    }
    .into()
}

/// What Agents: Yes means, in one line (the tag's tooltip, the menu's and the New Channel window's note).
pub(crate) const AGENTS_YES: &str = "Agents on every member's device can read and post here. Their messages are marked.";
/// What Agents: No means, in one line.
pub(crate) const AGENTS_NO: &str = "Agents can never read anything in this channel.";

/// A channel open to agents (Agents: Yes) has a tag ("agents"), the same in the sidebar and on its page; `None` for
/// a channel closed to them.
pub(crate) fn agent_tag(channel: &SyncChannel) -> Option<SidebarTag> {
    channel.agents.then(|| SidebarTag { text: "agents".into(), help: format!("Agents: Yes. {AGENTS_YES}") })
}

/// The tag of a channel open to agents for its page (its toolbar, by its name): "agents", with what that means in
/// its tooltip. `None` for a channel closed to them.
#[uniffi::export]
pub fn chat_agent_tag(channel: SyncChannel) -> Option<SidebarTag> {
    agent_tag(&channel)
}

/// One choice of a channel's Agents menu.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ChatAgentsChoice {
    /// "Yes" or "No".
    pub title: String,
    /// What choosing it sets (`chat_set_agents`).
    pub open: bool,
    /// The channel's setting now.
    pub checked: bool,
}

/// A channel's Agents: Yes/No in its ⋯ menu: one setting for the whole channel, which its owners and admins change.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ChatAgentsMenu {
    /// The menu's title: "Agents: Yes" or "Agents: No".
    pub title: String,
    /// Yes, then No.
    pub choices: Vec<ChatAgentsChoice>,
    /// This computer may change it (an owner or admin of the channel); else the choices are shown disabled.
    pub can_change: bool,
    /// One line under the choices: what the setting means, or why this computer cannot change it.
    pub note: String,
    /// The alert's title when the change fails ("Couldn't change whether agents may use “Bridge”"); its text is the
    /// error's sentence.
    pub failure_title: String,
}

/// A channel's Agents menu (its ⋯ menu): "Agents: Yes" with Yes and No, enabled for owners and admins.
pub(crate) fn agents_menu(channel: &SyncChannel) -> ChatAgentsMenu {
    let can_change = matches!(channel.role, SyncRole::Owner | SyncRole::Admin);
    let note = if can_change {
        (if channel.agents { AGENTS_YES } else { AGENTS_NO }).to_owned()
    } else {
        "Only the channel's owners and admins can change this.".to_owned()
    };
    ChatAgentsMenu {
        title: format!("Agents: {}", if channel.agents { "Yes" } else { "No" }),
        choices: [("Yes", true), ("No", false)].map(|(t, open)| ChatAgentsChoice { title: t.into(), open, checked: channel.agents == open }).to_vec(),
        can_change,
        note,
        failure_title: format!("Couldn't change whether agents may use “{}”", channel.name),
    }
}

/// A channel's Agents menu (its ⋯ menu): "Agents: Yes" with Yes and No, enabled for owners and admins.
#[uniffi::export]
pub fn chat_agents_menu(channel: SyncChannel) -> ChatAgentsMenu {
    agents_menu(&channel)
}

/// The New Channel window's line under its Agents choice: what Yes (`open`) or No means.
#[uniffi::export]
pub fn chat_new_agents_note(open: bool) -> String {
    format!("{} You can change it later in the channel's ⋯ menu.", if open { AGENTS_YES } else { AGENTS_NO })
}

/// A channel page's line under its name: "2 of 3 online · Muted · shared until 1 Nov". Its role is in the members
/// panel, and a channel open to agents has its tag in the toolbar. `share_until_day`: the channel's share end in the
/// user's own date style.
pub(crate) fn header(channel: &SyncChannel, share_until_day: Option<String>) -> String {
    let mut parts = vec![subtitle(channel)];
    if let Some(day) = share_until_day.filter(|_| channel.share_until_s.is_some()) {
        parts.push(format!("shared until {day}"));
    }
    parts.join(" · ")
}

/// A channel page's line: "2 of 3 online · Muted · shared until 1 Nov".
#[uniffi::export]
pub fn chat_header(channel: SyncChannel, share_until_day: Option<String>) -> String {
    header(&channel, share_until_day)
}

/// A topic page's line: "Topic by Kari's iPhone · 12 messages".
pub(crate) fn topic_line(topic: &SyncTopic) -> String {
    let author = if topic.author_name.is_empty() { "an unnamed device" } else { &topic.author_name };
    format!("Topic by {author} · {}", d::count(u64::from(topic.messages), "message"))
}

/// A topic page's line: "Topic by Kari's iPhone · 12 messages".
#[uniffi::export]
pub fn chat_topic_line(topic: SyncTopic) -> String {
    topic_line(&topic)
}

/// A private conversation's line: "End-to-end encrypted" while connected directly (the only way a private message
/// goes), else "Not connected".
#[uniffi::export]
pub fn chat_direct_line(online: bool) -> String {
    if online { "End-to-end encrypted" } else { "Not connected" }.into()
}

/// A channel's unread messages under General: its total less its topics' (the core counts the total), never below
/// zero while the counts catch up.
pub(crate) fn general_unread(channel: &SyncChannel, topics: &[SyncTopic]) -> u32 {
    let in_topics: u32 = topics.iter().map(|t| t.unread).fold(0, u32::saturating_add);
    channel.unread - channel.unread.min(in_topics)
}

/// A channel's unread messages under General: its total less its topics'.
#[uniffi::export]
pub fn chat_general_unread(channel: SyncChannel, topics: Vec<SyncTopic>) -> u32 {
    general_unread(&channel, &topics)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn channel(id: &str, members: u32, online: u32, unread: u32, muted: bool) -> SyncChannel {
        SyncChannel {
            id: id.into(),
            name: id.into(),
            role: SyncRole::Owner,
            share_until_s: None,
            members,
            online,
            unread,
            last_message_s: None,
            muted,
            agents: false,
            wake_command: None,
            progress: None,
            problem: None,
            waiting: None,
            gone: Vec::new(),
        }
    }

    pub(crate) fn topic(id: &str, channel: &str, unread: u32) -> SyncTopic {
        SyncTopic {
            id: id.into(),
            channel_id: channel.into(),
            title: id.into(),
            author_name: "Kari's iPhone".into(),
            created_s: 0,
            last_message_s: None,
            unread,
            messages: 12,
        }
    }

    #[test]
    fn the_list_and_the_page_say_what_is_going_on() {
        let c = channel("Team", 3, 2, 0, false);
        assert_eq!(header(&c, None), "2 of 3 online", "the role is in the members panel");
        let mut m = channel("Team", 3, 2, 0, true);
        m.agents = true;
        assert_eq!(header(&m, None), "2 of 3 online · Muted", "the agents tag is in the toolbar");
        m.share_until_s = Some(1_800_000_000);
        assert_eq!(header(&m, Some("1 Nov".into())), "2 of 3 online · Muted · shared until 1 Nov");
        assert_eq!(subtitle(&c), "2 of 3 online");
        assert_eq!(chat_agents_label(SyncAgentAccess::ReadPost), "Read and post");
    }

    #[test]
    fn a_channel_open_to_agents_has_a_tag() {
        let plain = channel("Family", 2, 1, 0, false);
        assert_eq!(agent_tag(&plain), None, "Agents: No: no tag");
        let mut c = channel("Bridge", 2, 2, 0, false);
        c.agents = true;
        let tag = chat_agent_tag(c.clone()).unwrap();
        assert_eq!(tag.text, "agents");
        assert!(tag.help.starts_with("Agents: Yes. Agents on every member's device") && tag.help.contains("marked"), "{}", tag.help);
    }

    #[test]
    fn the_agents_menu_is_yes_or_no_and_only_owners_and_admins_change_it() {
        let mut c = channel("Bridge", 2, 2, 0, false);
        let m = chat_agents_menu(c.clone());
        assert_eq!((m.title.as_str(), m.can_change), ("Agents: No", true));
        assert_eq!(m.choices.iter().map(|x| (x.title.as_str(), x.open, x.checked)).collect::<Vec<_>>(), [("Yes", true, false), ("No", false, true)]);
        assert!(m.note.contains("never read"), "{}", m.note);
        assert_eq!(m.failure_title, "Couldn't change whether agents may use “Bridge”");
        c.agents = true;
        c.role = SyncRole::Admin;
        let m = chat_agents_menu(c.clone());
        assert_eq!((m.title.as_str(), m.can_change, m.choices[0].checked), ("Agents: Yes", true, true));
        assert!(m.note.contains("read and post"), "{}", m.note);
        for r in [SyncRole::ReadWrite, SyncRole::ReadOnly] {
            c.role = r;
            let m = chat_agents_menu(c.clone());
            assert!(!m.can_change && m.note == "Only the channel's owners and admins can change this.", "{r:?}");
        }
        assert!(chat_new_agents_note(true).starts_with("Agents on every member's device can read and post here."));
        assert!(chat_new_agents_note(false).starts_with("Agents can never read") && chat_new_agents_note(false).ends_with("⋯ menu."));
        assert_eq!(chat_agents_label(SyncAgentAccess::Read), "Read", "private messages keep their levels");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_channel_of_one_mac_says_so() {
        assert_eq!(subtitle(&channel("Solo", 1, 0, 0, true)), "Only this Mac · Muted");
        let alone = SyncChannel { waiting: Some("Waiting for Mac mini (last seen 3 days ago)".into()), ..channel("Plans", 2, 1, 0, false) };
        assert_eq!(subtitle(&alone), "Waiting for Mac mini (last seen 3 days ago)", "whom it waits for, from the core");
        assert_eq!(header(&alone, None), "Waiting for Mac mini (last seen 3 days ago)");
    }

    #[test]
    fn devices_are_named_with_their_user() {
        assert_eq!(named("Studio iMac", "Kari"), "Kari (Studio iMac)");
        assert_eq!(named("Studio iMac", ""), "Studio iMac");
        assert_eq!(named("", "Kari"), "Kari (Unnamed device)");
        assert_eq!(named("", ""), "Unnamed device");
    }

    #[test]
    fn topics_and_private_conversations() {
        assert_eq!(topic_line(&topic("Dinner", "Team", 0)), "Topic by Kari's iPhone · 12 messages");
        let mut nameless = topic("Dinner", "Team", 0);
        nameless.author_name.clear();
        nameless.messages = 1;
        assert_eq!(topic_line(&nameless), "Topic by an unnamed device · 1 message");
        assert_eq!(chat_direct_line(false), "Not connected");
        assert_eq!(chat_direct_line(true), "End-to-end encrypted");
        let topics = [topic("a", "Team", 1), topic("b", "Team", 2)];
        assert_eq!(general_unread(&channel("Team", 2, 1, 5, false), &topics), 2);
        assert_eq!(general_unread(&channel("Team", 2, 1, 1, false), &topics), 0, "never below zero while the counts catch up");
    }
}
