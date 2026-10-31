//! Channels' words (SPEC 7G.7) that the window's sidebar and title bar, the conversation pages and the notifications
//! share (docs/CROSS-PLATFORM.md §1): a device by its name and user, a channel's and a topic's grey lines, the unread
//! count of a channel's General, and what agents may do.
//!
//! A date in the user's own style ("1 Nov") stays the host's: the functions that show one take it already written
//! (`…_day`), like the sync texts.

use trs_control::display as d;

use crate::sidebar::SidebarTag;
use crate::sync::{SyncAgentAccess, SyncChannel, SyncTopic};
use crate::sync_text::role;

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

/// A channel's grey line in the list: "2 of 3 online", "Only this Mac · Muted".
pub(crate) fn subtitle(channel: &SyncChannel) -> String {
    let mut parts = vec![if channel.members > 1 {
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

/// What agents on this computer may do in a channel: "Off", "Read", "Read and post".
#[uniffi::export]
pub fn chat_agents_label(access: SyncAgentAccess) -> String {
    match access {
        SyncAgentAccess::Off => "Off",
        SyncAgentAccess::Read => "Read",
        SyncAgentAccess::ReadPost => "Read and post",
    }
    .into()
}

/// An agent channel's tag ("agents"), the same in the sidebar and on its page; `None` for any other channel.
pub(crate) fn agent_tag(channel: &SyncChannel) -> Option<SidebarTag> {
    channel.agent_channel.then(|| SidebarTag {
        text: "agents".into(),
        help: format!(
            concat!("Agent channel: an agent made it, so agents can talk where you can read. Agents on this ", trs_platform::computer!(), ": {} (⋯ ▸ Agents on This ", trs_platform::computer!(), ")"),
            chat_agents_label(channel.agents)
        ),
    })
}

/// An agent channel's tag for its page (its toolbar, by its name): "agents", with what agents on this computer may
/// do in its tooltip. `None` for any other channel.
#[uniffi::export]
pub fn chat_agent_tag(channel: SyncChannel) -> Option<SidebarTag> {
    agent_tag(&channel)
}

/// A channel page's line under its name: "Owner · 2 of 3 online · shared until 1 Nov · agents: read", led by
/// "Agent channel" for one. `share_until_day`: the channel's share end in the user's own date style.
pub(crate) fn header(channel: &SyncChannel, share_until_day: Option<String>) -> String {
    let mut parts = vec![role(channel.role).to_owned(), subtitle(channel)];
    if channel.agent_channel {
        parts.insert(0, "Agent channel".into());
    }
    if let Some(day) = share_until_day.filter(|_| channel.share_until_s.is_some()) {
        parts.push(format!("shared until {day}"));
    }
    if channel.agents != SyncAgentAccess::Off {
        parts.push(format!("agents: {}", chat_agents_label(channel.agents).to_lowercase()));
    }
    parts.join(" · ")
}

/// A channel page's line: "Owner · 2 of 3 online · shared until 1 Nov · agents: read".
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

/// A private conversation's line: "Private · connected · end-to-end encrypted" (connected directly: the only way a
/// private message goes).
#[uniffi::export]
pub fn chat_direct_line(online: bool) -> String {
    format!("Private · {} · end-to-end encrypted", if online { "connected" } else { "not connected" })
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
    use crate::sync::SyncRole;

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
            agents: SyncAgentAccess::Off,
            agent_channel: false,
            wake_command: None,
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
        assert_eq!(header(&c, None), "Owner · 2 of 3 online");
        let mut m = channel("Team", 3, 2, 0, true);
        m.agents = SyncAgentAccess::Read;
        assert_eq!(header(&m, None), "Owner · 2 of 3 online · Muted · agents: read");
        m.share_until_s = Some(1_800_000_000);
        assert_eq!(header(&m, Some("1 Nov".into())), "Owner · 2 of 3 online · Muted · shared until 1 Nov · agents: read");
        assert_eq!(subtitle(&c), "2 of 3 online");
        assert_eq!(chat_agents_label(SyncAgentAccess::ReadPost), "Read and post");
    }

    #[test]
    fn an_agent_channel_has_a_tag_and_says_so_on_its_page() {
        let plain = channel("Family", 2, 1, 0, false);
        assert_eq!(agent_tag(&plain), None, "the user's own channel: no tag");
        let mut c = channel("Bridge", 2, 2, 0, false);
        c.agent_channel = true;
        c.agents = SyncAgentAccess::ReadPost;
        let tag = chat_agent_tag(c.clone()).unwrap();
        assert_eq!(tag.text, "agents");
        assert!(tag.help.starts_with("Agent channel: an agent made it") && tag.help.contains(": Read and post (⋯ ▸ Agents on This "), "{}", tag.help);
        assert_eq!(header(&c, None), "Agent channel · Owner · 2 of 2 online · agents: read and post");
        // Closed by this computer's user: still an agent channel, and the tag says what agents may do now.
        c.agents = SyncAgentAccess::Off;
        assert!(agent_tag(&c).unwrap().help.contains(": Off (⋯"));
        assert_eq!(header(&c, None), "Agent channel · Owner · 2 of 2 online");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_channel_of_one_mac_says_so() {
        assert_eq!(subtitle(&channel("Solo", 1, 0, 0, true)), "Only this Mac · Muted");
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
        assert_eq!(chat_direct_line(false), "Private · not connected · end-to-end encrypted");
        assert_eq!(chat_direct_line(true), "Private · connected · end-to-end encrypted");
        let topics = [topic("a", "Team", 1), topic("b", "Team", 2)];
        assert_eq!(general_unread(&channel("Team", 2, 1, 5, false), &topics), 2);
        assert_eq!(general_unread(&channel("Team", 2, 1, 1, false), &topics), 0, "never below zero while the counts catch up");
    }
}
