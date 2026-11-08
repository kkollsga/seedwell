//! A conversation's page (SPEC 7G.7) as a view model every host draws (docs/CROSS-PLATFORM.md §1): a channel's General
//! or one of its topics, or a private conversation. Rust decides the toolbar (the agents tag, Members, Share… and the ⋯
//! menu with the questions it asks first), the message cards (who wrote them, runs of one author, the time, replies,
//! mail headers, tags, the Markdown as runs, the images and files), the members panel with its roles and gone
//! devices, and the composer (whether it may post, its words, when Send is enabled, what Return does). The host draws,
//! reads for it ([`Core::chat_page_read`], `Core::sync_members`) and runs the [`ChatPageCommand`]s.
//!
//! - [`Core::chat_page_read`]: the conversation's latest messages and pending sends (shown messages count as read).
//! - [`chat_page_model`]: the page.
//! - [`chat_composer_can_send`], [`chat_composer_body`], [`chat_composer_key`]: the composer's rules while typing.
//! - [`chat_message_preview`], [`chat_message_author`], [`chat_notification_text`]: one-line words for banners.
//!
//! The page's title and line under it are the library's (`LibraryViewModel::title`, `summary`).

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use seedwell_platform::computer;

use crate::chat_dialogs::{chat_delete_conversation_dialog, chat_leave_dialog, chat_new_topic_dialog, chat_rename_dialog, chat_wake_command_dialog, ChatDialog};
use crate::chat_files::ChatPendingSend;
use crate::chat_groups::{chat_new_group_dialog, SyncChatGroup};
use crate::chat_images::ChatImage;
use crate::chat_markdown::{blocks, plain, ChatBlock, ChatRun};
use crate::chat_text::{agent_tag, agents_menu, chat_agents_label, general_unread, named, UNNAMED_DEVICE};
use crate::devices_text::{follow_line_for, FollowLine};
use crate::host::{has, HostWindow};
use crate::library::{local_day, local_day_number, LibraryPlace, LocalClock};
use crate::mail::{chat_cards, chat_mail, chat_tag_label, ChatMail, ChatTagOwner};
use crate::sidebar::SidebarTag;
use crate::sync::{SyncAgentAccess, SyncChannel, SyncDirect, SyncJoined, SyncMember, SyncMessage, SyncMessageKind, SyncRole, SyncTopic};
use crate::sync_page::SyncQuestion;
use crate::sync_text::{role, sync_can_share, sync_gone_prompt, sync_members_ordered};
use crate::Core;

/// How many of the newest messages a page shows (the core keeps them all; there is no paging back yet).
pub(crate) const PAGE_MESSAGES: u32 = 300;
/// A run of messages from one author within this many seconds shows the author once.
const RUN_S: u64 = 5 * 60;

// MARK: The records

/// What an item of the page does. After a change the host reads again ([`Core::chat_page_read`], and the members after
/// a change of roles or members) and asks for the model again. A failure is told in an alert: an item with a `dialog`
/// under its `failure_title`, the members' commands with `sync_set_role_failed` / `sync_remove_device_failed`, the
/// rest under the words given below; the alert's text is `core_error_message(err)`.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum ChatPageCommand {
    /// Opens the members panel: read `Core::sync_members(channel)` off the UI thread, then draw
    /// [`ChatPageModel::members`] (the model of that read).
    ShowMembers,
    /// The channel's Share dialog (`HostWindow::ChannelShare`: `Core::sync_share_form(channel)`).
    Share,
    /// `Core::chat_set_muted(channel, muted)`.
    SetMuted { muted: bool },
    /// `Core::chat_set_agents(channel, open)`; a failure under `ChatAgentsMenu::failure_title` (`chat_agents_menu`).
    SetAgents { open: bool },
    /// `Core::sync_set_follow(channel, on)`; a failure alone, as its sentence.
    SetFollow { on: bool },
    /// The item's dialog, then `Core::chat_set_wake_command(channel, value)` (`chat_dialog_check`).
    WakeCommand,
    /// The item's dialog, then `Core::chat_new_topic(channel, value)` and the new topic's page.
    NewTopic,
    /// The item's dialog, then `Core::chat_rename(channel, value)`.
    Rename,
    /// The item's dialog, then `Core::chat_leave(channel)`.
    Leave,
    /// `Core::chat_set_direct_agents(access)` (one setting for every private conversation on this computer).
    SetDirectAgents { access: SyncAgentAccess },
    /// The item's dialog, then `Core::chat_delete_direct(device)` and the Folder Sync page.
    DeleteConversation,
    /// `Core::sync_set_role(channel, device_id, role)`.
    SetRole { device_id: String, role: SyncRole },
    /// `Core::sync_remove_device(channel, device_id)`: a member, or a device gone from the channel.
    RemoveDevice { device_id: String },
    /// Closes the members panel and opens the private conversation with the device (`LibraryPlace::Direct`), named
    /// `name` until it has a message (`Library::note_direct_name`).
    MessagePrivately { device_id: String, name: String },
    /// The composer replies to this message (its card's `replying` line shows above the field) and takes the focus.
    Reply { message_id: String },
    /// Your tags on the message are now `tags` (the whole set): send `chat_tag_note(message_id, tags)` to the
    /// conversation as a message (`Core::chat_send` / `Core::chat_send_direct`, no reply); others keep theirs.
    SetTags { message_id: String, tags: Vec<String> },
    /// `Core::chat_set_group(channel, group_id)`: the channel listed under a sidebar group (`None`: under none); a
    /// failure under `chat_group_failure_title(true)`.
    MoveToGroup { group_id: Option<String> },
    /// The item's dialog (`chat_new_group_dialog`), then `Core::chat_group_new(value, None, Some(channel))`.
    NewGroup,
}

/// One line of a menu of the page.
// An item carries its dialog by value: a host reads the record across the FFI, where a box would not help.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum ChatPageMenuEntry {
    Item {
        title: String,
        command: ChatPageCommand,
        /// Drawn with a check mark.
        checked: bool,
        /// An on/off item (a toggle: On All My Devices, a tag) rather than a choice among several.
        toggle: bool,
        enabled: bool,
        /// Drawn in the warning colour.
        destructive: bool,
        /// A tooltip, for a host whose menus show one (empty: none).
        help: String,
        /// Asked first: the command runs with what its action gives (`chat_dialog_check`).
        dialog: Option<ChatDialog>,
    },
    Submenu {
        title: String,
        entries: Vec<ChatPageMenuEntry>,
    },
    /// A line of quiet text that cannot be chosen (what a setting means).
    Note { text: String },
    Separator,
}

/// A menu: its title (empty for the ⋯ menu, drawn as that symbol), its tooltip and its entries.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ChatPageMenu {
    pub title: String,
    pub help: String,
    pub entries: Vec<ChatPageMenuEntry>,
}

/// A button of the page.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ChatPageButton {
    /// Its title; for one drawn as a symbol (Reply, Message Privately), what it is.
    pub title: String,
    pub help: String,
    /// What a screen reader says when it is not the title.
    pub accessibility_label: Option<String>,
    pub command: ChatPageCommand,
    /// Asked first: the command runs on its confirming button.
    pub question: Option<SyncQuestion>,
}

/// The page's controls in the window's toolbar, after its title: the agents tag, Members, Share… and the ⋯ menu.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ChatPageToolbar {
    /// A channel open to agents says so by its name ("agents", with its tooltip).
    pub tag: Option<SidebarTag>,
    /// A channel's Members (opens the members panel under it).
    pub members: Option<ChatPageButton>,
    /// A channel's Share… (this computer may share it, and the host has `ChannelShare`).
    pub share: Option<ChatPageButton>,
    /// The ⋯ menu.
    pub menu: ChatPageMenu,
}

/// Whose a message is, which decides how it is drawn: the user's own and their agents' as one family (a thin accent
/// edge and a faint tint in a channel, the right side in a private conversation), other people's and their agents'
/// plain, with a round initial or the agent symbol at the start of a run. An agent's always carries the agent symbol.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum ChatAuthorKind {
    /// The user wrote it, on this device or one of their own devices (Settings › Devices, "Your devices").
    Mine,
    /// An agent posted it on this device or one of the user's own devices.
    OurAgent,
    /// Another person (another device) wrote it.
    Theirs,
    /// An agent on another person's device posted it.
    TheirAgent,
}

/// Who wrote a run of messages, on one small quiet line under its last message (under the bubble, on its side, in a
/// private conversation): "Kari · Kari's iPhone · 14.32", "Seedwell · Studio iMac · 14.32" (an agent, after the agent
/// symbol), "You · 14.32" on this device, "You · Kristian's iPhone · 14.32" on another of the user's devices.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ChatMessageMeta {
    /// The author, a little stronger than the rest: "You", "Kari", "Studio iMac", "Seedwell".
    pub name: String,
    /// After the name and " · ": the device (left out when it is this one, or the name already) and the time of the
    /// run's last message ("14.32" today, "Yesterday 14.32", "1 Nov 14.32" before).
    pub detail: String,
    /// The letter of another person's round initial: "K".
    pub initial: String,
    /// The line's tooltip: "Seedwell, an agent on Studio iMac", "Kari on Kari's iPhone"; empty for your own here.
    pub help: String,
}

/// A quoted message: a sent reply's quote (a small inset block with the quoted author's edge colour: "Kari",
/// "Middag på lørdag?"; a click scrolls to the original and lights it briefly) and the composer's reply bar
/// ("Replying to Kari", "Middag på lørdag?").
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ChatQuote {
    pub title: String,
    /// The message without its Markdown, on one line.
    pub text: String,
    /// The quoted message (its card's `id`), and whose it is with its colour, as on its own card.
    pub message_id: String,
    pub who: ChatAuthorKind,
    #[uniffi(default = None)]
    pub palette: Option<u32>,
}

/// How many calm colours the edges of other people's messages take (`ChatMessageCard::palette`); the host maps each
/// index to a soft, low-saturation colour that works in dark and light, away from the accent's hue and from red and
/// green.
pub const CHAT_PALETTE_SIZE: u32 = 8;

/// Each person's colour in a channel, by device: people (a user's devices together, else a device alone) take the
/// palette's colours in the order they joined the channel, and keep theirs for good (one who left keeps their place,
/// so nobody else's colour changes). Up to [`CHAT_PALETTE_SIZE`] people never share a colour; from the next one on
/// they repeat. Built from the membership every member device holds, so every device shows the same colours. The
/// user counts as a person too (their own messages show the accent), so that holds on every device.
pub(crate) fn palette_map(joined: &[SyncJoined]) -> HashMap<String, u32> {
    let mut order: Vec<&SyncJoined> = joined.iter().collect();
    order.sort_by(|a, b| (a.at_s, &a.device_id).cmp(&(b.at_s, &b.device_id)));
    let mut people: HashMap<String, u32> = HashMap::new();
    let mut out = HashMap::new();
    for j in order {
        let person = if j.user.is_empty() { format!("device:{}", j.device_id) } else { format!("user:{}", j.user) };
        let next = people.len() as u32;
        let index = *people.entry(person).or_insert(next);
        out.insert(j.device_id.clone(), index % CHAT_PALETTE_SIZE);
    }
    out
}

/// The colour of a message's edge: its person's in `map`; for a device the channel does not list (a private
/// conversation), one from its id (FNV-1a), the same everywhere. Their agents take the same colour, muted. `None` for
/// the user's own (the accent).
pub(crate) fn palette(m: &SyncMessage, who: ChatAuthorKind, map: &HashMap<String, u32>) -> Option<u32> {
    if matches!(who, ChatAuthorKind::Mine | ChatAuthorKind::OurAgent) {
        return None;
    }
    if let Some(&i) = map.get(&m.author_device) {
        return Some(i);
    }
    let mut h: u32 = 0x811c_9dc5;
    for b in m.author_device.bytes() {
        h = (h ^ u32::from(b)).wrapping_mul(0x0100_0193);
    }
    Some(h % CHAT_PALETTE_SIZE)
}

/// The number of colours of other people's edges ([`CHAT_PALETTE_SIZE`]).
#[uniffi::export]
pub fn chat_palette_size() -> u32 {
    CHAT_PALETTE_SIZE
}

/// The tooltip and accessibility name of a code block's copy button.
#[uniffi::export]
pub fn chat_code_copy_help() -> String {
    "Copy".into()
}

/// The colour of a mail's type chip.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum ChatMailTint {
    /// A bug.
    Red,
    /// A request or a question.
    Blue,
    /// A heads-up.
    Orange,
    /// A reply.
    Teal,
    /// Anything else: the quiet colour.
    Neutral,
}

/// The header of a mail between projects (SPEC 7G.8), drawn as a line above its text, never as text.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum ChatMailBanner {
    /// Bookkeeping as one quiet line (a seen receipt with an eye, a hook change with a bell).
    Note { line: String, seen: bool },
    /// A status: "Progress" (a clock, orange) or "Done" (a filled check mark, green), semibold.
    Status { title: String, done: bool },
    /// A mail: its type chip ("Bug", tinted), "to kglite" (an inbox symbol), "re 0392ba7c" (monospaced), then its
    /// summary (13.5 points, semibold).
    Mail { chip: Option<String>, tint: ChatMailTint, to: Option<String>, re: Option<String>, summary: Option<String> },
}

/// What a card shows as its text.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum ChatMessageBody {
    /// A text message's blocks (none for an empty caption of images or files).
    Markdown { blocks: Vec<ChatBlock> },
    /// A mail: its banner, then its text's blocks.
    Mail { banner: ChatMailBanner, blocks: Vec<ChatBlock> },
    /// A JSON message, monospaced and selectable.
    Json { text: String },
    /// A message this version cannot show: one quiet line.
    Unsupported { text: String },
}

/// One owner's tags on a message, as a quiet line under it: "kglite: Seen, Backlog" (an eye for a seen receipt
/// alone, else a tag symbol).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ChatTagLine {
    pub text: String,
    pub seen: bool,
}

/// A message. A run's first message stands apart from the one before (16 points on the Mac); the rest of the run
/// sits close (2). How it is drawn follows `who`.
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct ChatMessageCard {
    pub id: String,
    /// What its attachments are asked for by (`Core::chat_image_*`, `Core::chat_file_*`): the channel's id, or
    /// `dm:<device>`.
    pub place: String,
    /// Under the last message of a run only: who wrote the run, from where and when.
    pub meta: Option<ChatMessageMeta>,
    /// The first message of a run (it stands apart from the one before; the rest sit close).
    pub starts_run: bool,
    /// Whose it is.
    pub who: ChatAuthorKind,
    /// The colour of its left edge, for another person's message and their agents' (muted): an index below
    /// [`CHAT_PALETTE_SIZE`], stable per person. `None` for the user's and their agents' (the accent).
    pub palette: Option<u32>,
    /// Its time ("14.32", "Yesterday 14.32"), shown while the pointer is on it (without moving anything).
    pub time: String,
    /// The message it answers, one quiet line over it: "Kari" and "Middag på lørdag?".
    pub reply_quote: Option<ChatQuote>,
    pub body: ChatMessageBody,
    /// Its images and files, after the text: the image view (`chat_images`) and file cards (`chat_files`) as they are.
    pub images: Vec<ChatImage>,
    /// Its images are a gallery (more than one): one large at a time, in this order, with a filmstrip under it to
    /// switch (which one is shown is the host's); else its one image is shown alone.
    pub gallery: bool,
    pub files: Vec<crate::chat_files::ChatFileCard>,
    /// Its tags by owner, on one line under it.
    pub tags: Vec<ChatTagLine>,
    /// Your tags (a tag symbol in the pill over the message while the pointer is on it); `None` where you cannot post.
    pub tag_menu: Option<ChatPageMenu>,
    /// Reply (an arrow symbol in the same pill); `None` where you cannot post.
    pub reply: Option<ChatPageButton>,
    /// The composer's reply bar while it replies to this message: "Replying to Kari" and the message's line.
    pub replying: ChatQuote,
}

/// A member in the members panel.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ChatMemberRow {
    pub device_id: String,
    /// "Kari (Studio iMac)", "Kristian (MacBook) (this Mac)".
    pub name: String,
    /// The green dot (else a quiet one).
    pub online: bool,
    /// Its role ("Read & write"): plain text, or the title of `role_menu`.
    pub role: String,
    /// An owner's menu for another member: the roles (the one in force disabled), then Remove from Channel.
    pub role_menu: Option<ChatPageMenu>,
    /// Message Privately (an envelope symbol), for another member.
    pub message: Option<ChatPageButton>,
    /// A grey line under the name while this computer (an admin) asked the owners to remove it: "Removal asked: waiting
    /// for the owner", or why an owner's device refused (`SyncMember::removal`, as the folder page's member line).
    pub removal: Option<String>,
}

/// A device gone from the channel: the line and its Remove from Channel… (asks first).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ChatGoneRow {
    pub device_id: String,
    pub line: String,
    pub button: ChatPageButton,
}

/// A channel's members panel (a popover under Members on the Mac).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ChatMembersPanel {
    pub title: String,
    /// This computer first, then by role and name.
    pub rows: Vec<ChatMemberRow>,
    /// After a separator: the devices gone from the channel.
    pub gone: Vec<ChatGoneRow>,
}

/// The composer under the messages.
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct ChatComposer {
    /// It may post; else `cannot_post` shows instead of the field (quiet, the secondary size).
    pub can_post: bool,
    pub cannot_post: Option<String>,
    /// "Message Family", "Message Dinner", "Message Kari's iPhone".
    pub placeholder: String,
    /// Images and files may be attached (the attach button, paste and drop: `chat_image_texts`, `chat_image_check`).
    pub attach: bool,
    /// The conversation's sends whose files are being read, or that failed (`chat_files`), above the field.
    pub pending: Vec<ChatPendingSend>,
    pub send_title: String,
    /// "Send (Return; Shift-Return for a new line)".
    pub send_help: String,
    /// The tooltip of the reply bar's close button (Escape also cancels the reply).
    pub reply_cancel_help: String,
}

/// The whole page.
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct ChatPageModel {
    /// What its attachments go to and are asked for by: the channel's id, or `dm:<device>`.
    pub place: String,
    /// The topic shown (`None`: General, or a private conversation).
    pub topic: Option<String>,
    pub toolbar: ChatPageToolbar,
    /// Oldest first; the host keeps the newest in view (it scrolls to the last when it changes).
    pub messages: Vec<ChatMessageCard>,
    /// A private conversation (always one to one) is drawn as bubbles, the user's on the right; a channel as one
    /// column.
    pub bubbles: bool,
    /// The quiet line while there are no messages.
    pub empty: Option<String>,
    /// A channel's members panel, from the members last read.
    pub members: Option<ChatMembersPanel>,
    pub composer: ChatComposer,
    /// Changes whenever anything above does.
    pub signature: String,
}

/// What the core reads for a page ([`Core::chat_page_read`]).
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct ChatPageRead {
    /// The newest messages, oldest first.
    pub messages: Vec<SyncMessage>,
    pub pending: Vec<ChatPendingSend>,
    /// A channel's "On all my devices".
    pub follow: Option<FollowLine>,
    /// What agents on this computer may do with private messages.
    pub direct_agents: SyncAgentAccess,
    /// The user's own devices (Settings › Devices, "Your devices"), this one among them: what they and their agents
    /// write is the user's ([`ChatAuthorKind`]). Empty while this device is in no group.
    #[uniffi(default = [])]
    pub own_devices: Vec<String>,
    /// A channel's devices in the order they joined, also those that left: each person's colour.
    #[uniffi(default = [])]
    pub joined: Vec<SyncJoined>,
    /// The sidebar's groups of channels, in order (a channel's Move to Group).
    #[uniffi(default = [])]
    pub groups: Vec<SyncChatGroup>,
}

/// What the page is built from.
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct ChatPageInput {
    /// `LibraryPlace::Chat` or `LibraryPlace::Direct`.
    pub place: LibraryPlace,
    /// The page's title as the library says it (`LibraryViewModel::title`): a private conversation's device.
    pub title: String,
    /// The channel (`Core::chat_channels`) and the topic shown (`Core::chat_topics`); a private conversation's device
    /// (`Core::chat_directs`; `None` before its first message).
    pub channel: Option<SyncChannel>,
    pub topic: Option<SyncTopic>,
    pub direct: Option<SyncDirect>,
    pub read: ChatPageRead,
    /// The channel's members as last read (`Core::sync_members`, when the panel opens and after a change).
    pub members: Vec<SyncMember>,
    pub now_unix_s: u64,
}

/// What Return does in the message field.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum ChatComposerKey {
    /// Send the message (as the Send button).
    Send,
    /// A new line in the field (the field's own handling).
    NewLine,
}

/// A banner's title and text.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ChatNotificationText {
    pub title: String,
    pub body: String,
}

// MARK: Words

/// The device that sent it, by its name.
fn computer_name(m: &SyncMessage) -> &str {
    if m.author_name.is_empty() { UNNAMED_DEVICE } else { &m.author_name }
}

/// "You", "Kari (Studio iMac)", or for an agent its name and the computer it runs on: "Seedwell on Studio iMac".
pub(crate) fn author(m: &SyncMessage) -> String {
    match &m.agent {
        Some(agent) => format!("{agent} on {}", computer_name(m)),
        None if m.is_me => "You".into(),
        None => named(&m.author_name, &m.author_user),
    }
}

/// One line for previews, banners and reply quotes: "Bug: Recipe tool returns the wrong catalogue".
pub(crate) fn mail_line(mail: &ChatMail) -> String {
    if mail.is_tag {
        return format!("Tagged {}", mail.tags.iter().map(|t| chat_tag_label(t)).collect::<Vec<_>>().join(", "));
    }
    if mail.is_note {
        return mail.summary.clone().unwrap_or_else(|| "Mail hook".into());
    }
    if mail.is_status {
        return format!("{}: {}", if mail.state.as_deref() == Some("open") { "Progress" } else { "Done" }, plain(&mail.body));
    }
    let what = mail.summary.clone().unwrap_or_else(|| plain(&mail.body));
    match &mail.type_label {
        Some(t) => format!("{t}: {what}"),
        None => what,
    }
}

/// One line of a message, for notifications, replies and the list: at most `limit` characters.
pub(crate) fn preview(m: &SyncMessage, limit: usize) -> String {
    let text = match m.kind {
        SyncMessageKind::Text if m.body.is_empty() && !(m.images.is_empty() && m.files.is_empty()) => m
            .images
            .iter()
            .map(|i| i.caption.clone())
            .chain(m.files.iter().map(|f| format!("{} · {}", f.name, f.size_text)))
            .collect::<Vec<_>>()
            .join(", "),
        SyncMessageKind::Text => chat_mail(&m.body).map(|mail| mail_line(&mail)).unwrap_or_else(|| plain(&m.body)),
        SyncMessageKind::Json => "JSON message".into(),
        SyncMessageKind::Other => "A message this version cannot show".into(),
    };
    let one = text.replace('\n', " ");
    let one = one.trim_matches(|c: char| c.is_whitespace() && c != '\n');
    if one.chars().count() > limit {
        one.chars().take(limit.saturating_sub(1)).chain(std::iter::once('…')).collect()
    } else {
        one.to_owned()
    }
}

/// A message's time: "14.32" today, "Yesterday 14.32", "1 Nov 14.32" before (and for one dated ahead).
pub(crate) fn message_time(clock: &dyn LocalClock, at: u64, now: u64) -> String {
    let time = clock.time_of_day(at);
    let (day, today) = (local_day_number(clock, at), local_day_number(clock, now));
    if day == today {
        time
    } else if day == today - 1 {
        format!("Yesterday {time}")
    } else {
        format!("{} {time}", local_day(clock, at, now))
    }
}

/// A run of messages from one author (one device, one agent) within five minutes shows who wrote it once, under its
/// last message; an agent's message and a reply start their own.
pub(crate) fn starts_run(m: &SyncMessage, prev: Option<&SyncMessage>) -> bool {
    let Some(prev) = prev else { return true };
    prev.author_device != m.author_device || prev.agent != m.agent || m.at_s > prev.at_s + RUN_S || m.reply_to.is_some()
}

/// One line of a message (a reply quote, a notification): at most `limit` characters, without its Markdown.
#[uniffi::export]
pub fn chat_message_preview(message: SyncMessage, limit: u32) -> String {
    preview(&message, limit as usize)
}

/// Who wrote a message, in one line: "You", "Kari (Studio iMac)", "Seedwell on Studio iMac".
#[uniffi::export]
pub fn chat_message_author(message: SyncMessage) -> String {
    author(&message)
}

/// A conversation's name in a banner: "Team", "Team › Dinner".
#[uniffi::export]
pub fn chat_place_title(channel: SyncChannel, topic: Option<SyncTopic>) -> String {
    match topic {
        Some(t) => format!("{} › {}", channel.name, t.title),
        None => channel.name,
    }
}

/// The banner for a new message in the conversation `place_title` ([`chat_place_title`], or a private conversation's
/// device): "Team › Dinner" / "Studio iMac: Hello there".
#[uniffi::export]
pub fn chat_notification_text(place_title: String, message: SyncMessage) -> ChatNotificationText {
    ChatNotificationText { body: format!("{}: {}", author(&message), preview(&message, 200)), title: place_title }
}

/// Send is enabled: there is text (not only spaces and line ends) or an attachment.
#[uniffi::export]
pub fn chat_composer_can_send(draft: String, attachments: u32) -> bool {
    !draft.trim().is_empty() || attachments > 0
}

/// What is sent of the field's text: trimmed; `None` when nothing is left (a text-only send then does nothing).
#[uniffi::export]
pub fn chat_composer_body(draft: String) -> Option<String> {
    Some(draft.trim().to_owned()).filter(|t| !t.is_empty())
}

/// What Return does in the message field: it sends; with Shift or Option (Alt) held it starts a new line.
#[uniffi::export]
pub fn chat_composer_key(shift: bool, alt: bool) -> ChatComposerKey {
    if shift || alt { ChatComposerKey::NewLine } else { ChatComposerKey::Send }
}

#[cfg(target_os = "macos")]
const SEND_HELP: &str = "Send (Return; Shift-Return for a new line)";
#[cfg(not(target_os = "macos"))]
const SEND_HELP: &str = "Send (Enter; Shift+Enter for a new line)";

// MARK: Building it

/// A menu item being built: plain, enabled, unchecked until said otherwise.
struct MenuItem(ChatPageMenuEntry);

impl MenuItem {
    fn set(mut self, f: impl FnOnce(&mut bool, &mut bool, &mut bool, &mut String)) -> Self {
        if let ChatPageMenuEntry::Item { checked, toggle, enabled, help, .. } = &mut self.0 {
            f(checked, toggle, enabled, help);
        }
        self
    }
    fn checked(self, on: bool) -> Self {
        self.set(|c, _, _, _| *c = on)
    }
    fn toggle(self) -> Self {
        self.set(|_, t, _, _| *t = true)
    }
    fn enabled(self, on: bool) -> Self {
        self.set(|_, _, e, _| *e = on)
    }
    fn help(self, text: String) -> Self {
        self.set(|_, _, _, h| *h = text)
    }
    fn entry(self) -> ChatPageMenuEntry {
        self.0
    }
}

fn it(title: impl Into<String>, command: ChatPageCommand) -> MenuItem {
    MenuItem(ChatPageMenuEntry::Item {
        title: title.into(),
        command,
        checked: false,
        toggle: false,
        enabled: true,
        destructive: false,
        help: String::new(),
        dialog: None,
    })
}

fn item(title: impl Into<String>, command: ChatPageCommand) -> ChatPageMenuEntry {
    it(title, command).entry()
}

/// An item that asks with `dialog` first: left out when the host cannot draw the chat dialogs.
fn asking(title: &str, command: ChatPageCommand, dialog: ChatDialog, enabled: bool, host: &[HostWindow]) -> Option<ChatPageMenuEntry> {
    has(host, Some(HostWindow::ChatDialogs)).then(|| ChatPageMenuEntry::Item {
        title: title.into(),
        command,
        checked: false,
        toggle: false,
        enabled,
        destructive: false,
        help: String::new(),
        dialog: Some(dialog),
    })
}

fn button(title: impl Into<String>, help: impl Into<String>, command: ChatPageCommand) -> ChatPageButton {
    ChatPageButton { title: title.into(), help: help.into(), accessibility_label: None, command, question: None }
}

/// Separators only between items: none at either end, never two together.
fn tidy(entries: Vec<ChatPageMenuEntry>) -> Vec<ChatPageMenuEntry> {
    let mut out: Vec<ChatPageMenuEntry> = Vec::with_capacity(entries.len());
    for e in entries {
        if e == ChatPageMenuEntry::Separator && matches!(out.last(), None | Some(ChatPageMenuEntry::Separator)) {
            continue;
        }
        out.push(e);
    }
    if out.last() == Some(&ChatPageMenuEntry::Separator) {
        out.pop();
    }
    out
}

/// Move to Group ▸ of a channel's ⋯ menu: each group (checked on the channel's), New Group… (asks, then makes it with
/// the channel), Remove from Group while it is in one.
fn group_submenu(c: &SyncChannel, groups: &[SyncChatGroup], host: &[HostWindow]) -> ChatPageMenuEntry {
    let current = c.group.as_deref().filter(|g| groups.iter().any(|x| x.id == *g));
    let mut entries: Vec<ChatPageMenuEntry> =
        groups.iter().map(|g| it(g.name.clone(), ChatPageCommand::MoveToGroup { group_id: Some(g.id.clone()) }).checked(current == Some(g.id.as_str())).entry()).collect();
    entries.push(ChatPageMenuEntry::Separator);
    entries.extend(asking("New Group…", ChatPageCommand::NewGroup, chat_new_group_dialog(), true, host));
    if current.is_some() {
        entries.push(item("Remove from Group", ChatPageCommand::MoveToGroup { group_id: None }));
    }
    ChatPageMenuEntry::Submenu { title: "Move to Group".into(), entries: tidy(entries) }
}

/// A channel's ⋯ menu: Mute, Agents: Yes/No, On All My Devices, Wake Command…; New Topic…, Rename…, Move to Group ▸;
/// Leave Channel….
fn channel_menu(c: &SyncChannel, follow: Option<&FollowLine>, groups: &[SyncChatGroup], host: &[HostWindow]) -> ChatPageMenu {
    let agents = agents_menu(c);
    let mut choices: Vec<ChatPageMenuEntry> = agents
        .choices
        .iter()
        .map(|x| it(x.title.clone(), ChatPageCommand::SetAgents { open: x.open }).checked(x.checked).enabled(agents.can_change && !x.checked).entry())
        .collect();
    choices.push(ChatPageMenuEntry::Separator);
    choices.push(ChatPageMenuEntry::Note { text: agents.note.clone() });
    let mut v = vec![
        item(if c.muted { "Unmute" } else { "Mute" }, ChatPageCommand::SetMuted { muted: !c.muted }),
        ChatPageMenuEntry::Submenu { title: agents.title.clone(), entries: choices },
    ];
    if let Some(f) = follow.filter(|f| f.shown) {
        v.push(it(f.menu_title.clone(), ChatPageCommand::SetFollow { on: !f.on }).checked(f.on).toggle().enabled(f.enabled).help(f.line.clone()).entry());
    }
    v.extend(asking("Wake Command…", ChatPageCommand::WakeCommand, chat_wake_command_dialog(c.clone()), c.agents, host));
    v.push(ChatPageMenuEntry::Separator);
    v.extend(asking("New Topic…", ChatPageCommand::NewTopic, chat_new_topic_dialog(c.clone()), c.role != SyncRole::ReadOnly, host));
    v.extend(asking("Rename…", ChatPageCommand::Rename, chat_rename_dialog(c.clone()), true, host));
    if has(host, Some(HostWindow::ChatGroups))
        && let ChatPageMenuEntry::Submenu { entries, .. } = group_submenu(c, groups, host)
        && !entries.is_empty()
    {
        v.push(ChatPageMenuEntry::Submenu { title: "Move to Group".into(), entries });
    }
    v.push(ChatPageMenuEntry::Separator);
    v.extend(asking("Leave Channel…", ChatPageCommand::Leave, chat_leave_dialog(c.clone()), true, host));
    ChatPageMenu { title: String::new(), help: "More".into(), entries: tidy(v) }
}

/// A private conversation's ⋯ menu: what agents on this computer may do with private messages; Delete Conversation….
fn direct_menu(name: &str, access: SyncAgentAccess, host: &[HostWindow]) -> ChatPageMenu {
    let levels = [SyncAgentAccess::Off, SyncAgentAccess::Read, SyncAgentAccess::ReadPost]
        .into_iter()
        .map(|a| it(chat_agents_label(a), ChatPageCommand::SetDirectAgents { access: a }).checked(a == access).entry())
        .collect();
    let mut v = vec![ChatPageMenuEntry::Submenu { title: concat!("Agents on This ", computer!()).into(), entries: levels }, ChatPageMenuEntry::Separator];
    v.extend(asking("Delete Conversation…", ChatPageCommand::DeleteConversation, chat_delete_conversation_dialog(name.to_owned()), true, host));
    ChatPageMenu { title: String::new(), help: "More".into(), entries: tidy(v) }
}

fn tint(kind: Option<&str>) -> ChatMailTint {
    match kind {
        Some("bug") => ChatMailTint::Red,
        Some("request" | "question") => ChatMailTint::Blue,
        Some("heads-up") => ChatMailTint::Orange,
        Some("reply") => ChatMailTint::Teal,
        _ => ChatMailTint::Neutral,
    }
}

fn banner(mail: &ChatMail) -> ChatMailBanner {
    if mail.is_note {
        ChatMailBanner::Note { line: mail_line(mail), seen: mail.kind.as_deref() == Some("seen") }
    } else if mail.is_status {
        let open = mail.state.as_deref() == Some("open");
        ChatMailBanner::Status { title: if open { "Progress" } else { "Done" }.into(), done: !open }
    } else {
        ChatMailBanner::Mail {
            chip: mail.type_label.clone(),
            tint: tint(mail.kind.as_deref()),
            to: mail.to.as_ref().map(|t| format!("to {t}")),
            re: mail.re.as_ref().map(|r| format!("re {}", r.chars().take(8).collect::<String>())),
            summary: mail.summary.clone(),
        }
    }
}

/// `blocks` with their image references checked against the `images` the message carries: one to an image it does not
/// carry is plain text (its description).
fn with_images(mut blocks: Vec<ChatBlock>, images: usize) -> Vec<ChatBlock> {
    let check = |runs: &mut Vec<ChatRun>| {
        let mut out: Vec<ChatRun> = Vec::with_capacity(runs.len());
        for mut r in runs.drain(..) {
            if r.image.is_some_and(|i| i as usize >= images) {
                r.image = None;
            }
            // Text again: it joins the text beside it.
            match out.last_mut() {
                Some(last) if r.image.is_none() && last.image.is_none() && ChatRun { text: String::new(), ..last.clone() } == ChatRun { text: String::new(), ..r.clone() } => {
                    last.text.push_str(&r.text)
                }
                _ => out.push(r),
            }
        }
        *runs = out;
    };
    for b in &mut blocks {
        match b {
            ChatBlock::Heading { runs, .. } | ChatBlock::Paragraph { runs } | ChatBlock::Item { runs, .. } | ChatBlock::Quote { runs } => check(runs),
            ChatBlock::Table { header, rows } => {
                header.iter_mut().chain(rows.iter_mut().flat_map(|r| r.cells.iter_mut())).for_each(|c| check(&mut c.runs));
            }
            ChatBlock::CodeBlock { .. } | ChatBlock::Rule => {}
        }
    }
    blocks
}

fn body(m: &SyncMessage) -> ChatMessageBody {
    let blocks = |text: &str| with_images(blocks(text), m.images.len());
    match m.kind {
        SyncMessageKind::Text => match chat_mail(&m.body) {
            Some(mail) => ChatMessageBody::Mail { banner: banner(&mail), blocks: blocks(&mail.body) },
            None => ChatMessageBody::Markdown { blocks: blocks(&m.body) },
        },
        SyncMessageKind::Json => ChatMessageBody::Json { text: m.body.clone() },
        SyncMessageKind::Other => ChatMessageBody::Unsupported { text: preview(m, 120) },
    }
}

/// Whose `m` is: the user's when it came from this device or one of `own` (the user's devices).
pub(crate) fn author_kind(m: &SyncMessage, own: &[String]) -> ChatAuthorKind {
    let ours = m.is_me || own.iter().any(|d| d == &m.author_device);
    match (ours, m.agent.is_some()) {
        (true, false) => ChatAuthorKind::Mine,
        (true, true) => ChatAuthorKind::OurAgent,
        (false, false) => ChatAuthorKind::Theirs,
        (false, true) => ChatAuthorKind::TheirAgent,
    }
}

/// A message's author in a few letters: "You", "Seedwell" (an agent), "Kari", else the device: "Studio iMac".
fn short_author(m: &SyncMessage, who: ChatAuthorKind) -> String {
    match (&m.agent, who) {
        (Some(agent), _) => agent.clone(),
        (None, ChatAuthorKind::Mine) => "You".into(),
        (None, _) if !m.author_user.is_empty() => m.author_user.clone(),
        (None, _) => computer_name(m).to_owned(),
    }
}

/// The line under a run whose last message is `m`.
fn message_meta(m: &SyncMessage, who: ChatAuthorKind, clock: &dyn LocalClock, now: u64) -> ChatMessageMeta {
    let name = short_author(m, who);
    let computer = computer_name(m);
    let here = m.is_me && m.agent.is_none();
    let help = match who {
        ChatAuthorKind::OurAgent | ChatAuthorKind::TheirAgent => format!("{name}, an agent on {computer}"),
        ChatAuthorKind::Mine if here => String::new(),
        ChatAuthorKind::Mine => format!("You on {computer}"),
        ChatAuthorKind::Theirs if name == computer => String::new(),
        ChatAuthorKind::Theirs => format!("{name} on {computer}"),
    };
    let time = message_time(clock, m.at_s, now);
    let detail = if here || name == computer { time } else { format!("{computer} · {time}") };
    let initial = name.chars().find(|c| c.is_alphanumeric()).map_or_else(|| "?".into(), |c| c.to_uppercase().collect());
    ChatMessageMeta { name, detail, initial, help }
}

/// Your tags on a message: the usual ones to switch on and off, and clearing them.
fn tag_menu(id: &str, owners: &[ChatTagOwner]) -> ChatPageMenu {
    let yours: Vec<String> = owners.iter().find(|o| o.is_you).map(|o| o.tags.clone()).unwrap_or_default();
    let set = |tags: Vec<String>| ChatPageCommand::SetTags { message_id: id.to_owned(), tags };
    let mut entries: Vec<ChatPageMenuEntry> = seedwell_control::mail::KNOWN
        .iter()
        .map(|&t| {
            let on = yours.iter().any(|y| y == t);
            let next = if on { yours.iter().filter(|y| *y != t).cloned().collect() } else { yours.iter().cloned().chain([t.to_owned()]).collect() };
            it(chat_tag_label(t), set(next)).checked(on).toggle().entry()
        })
        .collect();
    if !yours.is_empty() {
        entries.push(ChatPageMenuEntry::Separator);
        entries.push(item("Clear My Tags", set(Vec::new())));
    }
    ChatPageMenu { title: String::new(), help: "Tag".into(), entries }
}

/// The messages as cards: tag notes are lines on the message they are about, not cards.
pub(crate) fn cards(messages: Vec<SyncMessage>, own: &[String], joined: &[SyncJoined], can_post: bool, clock: &dyn LocalClock, now: u64) -> Vec<ChatMessageCard> {
    let colours = palette_map(joined);
    let c = chat_cards(messages);
    let tags: HashMap<String, Vec<ChatTagOwner>> = c.tags.into_iter().map(|t| (t.message, t.owners)).collect();
    let ms = c.cards;
    ms.iter()
        .enumerate()
        .map(|(n, m)| {
            let owners = tags.get(&m.id.chars().take(8).collect::<String>().to_lowercase()).map(Vec::as_slice).unwrap_or_default();
            let who = author_kind(m, own);
            ChatMessageCard {
                id: m.id.clone(),
                place: m.place.clone(),
                meta: ms.get(n + 1).is_none_or(|next| starts_run(next, Some(m))).then(|| message_meta(m, who, clock, now)),
                starts_run: starts_run(m, n.checked_sub(1).map(|p| &ms[p])),
                who,
                palette: palette(m, who, &colours),
                time: message_time(clock, m.at_s, now),
                reply_quote: m.reply_to.as_ref().and_then(|r| ms.iter().find(|x| &x.id == r)).map(|r| {
                    let rw = author_kind(r, own);
                    ChatQuote { title: short_author(r, rw), text: preview(r, 120), message_id: r.id.clone(), who: rw, palette: palette(r, rw, &colours) }
                }),
                body: body(m),
                images: m.images.clone(),
                gallery: m.images.len() > 1,
                files: m.files.clone(),
                tags: owners
                    .iter()
                    .map(|o| ChatTagLine {
                        text: format!("{}: {}", o.owner, o.tags.iter().map(|t| chat_tag_label(t)).collect::<Vec<_>>().join(", ")),
                        seen: o.tags == ["seen"],
                    })
                    .collect(),
                tag_menu: can_post.then(|| tag_menu(&m.id, owners)),
                reply: can_post.then(|| button("Reply", "Reply", ChatPageCommand::Reply { message_id: m.id.clone() })),
                replying: ChatQuote {
                    title: format!("Replying to {}", short_author(m, who)),
                    text: preview(m, 120),
                    message_id: m.id.clone(),
                    who,
                    palette: palette(m, who, &colours),
                },
            }
        })
        .collect()
}

/// The roles an owner hands out, in the menu's order.
const ROLES: [SyncRole; 4] = [SyncRole::Owner, SyncRole::Admin, SyncRole::ReadWrite, SyncRole::ReadOnly];

fn members_panel(c: &SyncChannel, members: &[SyncMember], now: u64) -> ChatMembersPanel {
    let rows = sync_members_ordered(members.to_vec())
        .into_iter()
        .map(|m| {
            let name = named(&m.name, &m.user);
            // An owner changes roles and removes anyone; an admin removes a user (Read & write, Read only).
            let owner = c.role == SyncRole::Owner;
            let admin_removes = c.role == SyncRole::Admin && crate::sync_page::admin_may_remove(m.role);
            let role_menu = (!m.is_me && (owner || admin_removes)).then(|| {
                let mut entries: Vec<ChatPageMenuEntry> = Vec::new();
                if owner {
                    entries.extend(ROLES.iter().map(|&r| it(role(r), ChatPageCommand::SetRole { device_id: m.device_id.clone(), role: r }).enabled(r != m.role).entry()));
                    entries.push(ChatPageMenuEntry::Separator);
                }
                // An admin's request waits for the owner: not asked twice.
                entries.push(it("Remove from Channel", ChatPageCommand::RemoveDevice { device_id: m.device_id.clone() }).enabled(owner || !m.removal_waiting).entry());
                ChatPageMenu { title: role(m.role).into(), help: String::new(), entries }
            });
            ChatMemberRow {
                device_id: m.device_id.clone(),
                name: if m.is_me { format!(concat!("{} (this ", computer!(), ")"), name) } else { name.clone() },
                online: m.online || m.is_me,
                role: role(m.role).into(),
                role_menu,
                message: (!m.is_me).then(|| ChatPageButton {
                    accessibility_label: Some("Message Privately".into()),
                    ..button("Message Privately", format!("Message {name} privately"), ChatPageCommand::MessagePrivately { device_id: m.device_id.clone(), name })
                }),
                removal: m.removal.clone(),
            }
        })
        .collect();
    let gone = c
        .gone
        .iter()
        .map(|g| {
            let p = sync_gone_prompt(g.clone(), c.name.clone(), true, now);
            ChatGoneRow {
                device_id: g.device_id.clone(),
                line: p.line,
                button: ChatPageButton {
                    accessibility_label: Some(p.help.clone()),
                    question: Some(SyncQuestion { title: p.title, text: p.text, cancel_title: "Cancel".into(), confirm_title: p.confirm, destructive: true }),
                    ..button(p.button, p.help, ChatPageCommand::RemoveDevice { device_id: g.device_id.clone() })
                },
            }
        })
        .collect();
    ChatMembersPanel { title: "Members".into(), rows, gone }
}

fn composer(can_post: bool, cannot_post: &str, name: &str, pending: Vec<ChatPendingSend>) -> ChatComposer {
    ChatComposer {
        can_post,
        cannot_post: (!can_post).then(|| cannot_post.to_owned()),
        placeholder: format!("Message {name}"),
        attach: can_post,
        pending,
        send_title: "Send".into(),
        send_help: SEND_HELP.into(),
        reply_cancel_help: "Cancel Reply (Esc)".into(),
    }
}

/// The page for `input`; `None` when its place is not a conversation, or its channel is gone (the library shows why).
/// `host`: the windows the host has (`host_windows_all()` on the Mac): Share… needs `ChannelShare`; New Topic…,
/// Rename…, Wake Command…, Leave Channel… and Delete Conversation… need `ChatDialogs`.
#[uniffi::export]
pub fn chat_page_model(input: ChatPageInput, clock: std::sync::Arc<dyn LocalClock>, host: Vec<HostWindow>) -> Option<ChatPageModel> {
    let now = input.now_unix_s;
    let mut m = match &input.place {
        LibraryPlace::Chat { channel_id, topic_id } => {
            let c = input.channel.as_ref().filter(|c| &c.id == channel_id)?;
            let can_post = c.role != SyncRole::ReadOnly;
            let name = input.topic.as_ref().map_or(c.name.as_str(), |t| t.title.as_str());
            let messages = cards(input.read.messages.clone(), &input.read.own_devices, &input.read.joined, can_post, clock.as_ref(), now);
            ChatPageModel {
                place: c.id.clone(),
                topic: topic_id.clone(),
                toolbar: ChatPageToolbar {
                    tag: agent_tag(c),
                    members: Some(button("Members", "Members", ChatPageCommand::ShowMembers)),
                    share: (sync_can_share(c.role) && has(&host, Some(HostWindow::ChannelShare)))
                        .then(|| button("Share…", concat!("Share: invite another ", computer!(), " to this channel"), ChatPageCommand::Share)),
                    menu: channel_menu(c, input.read.follow.as_ref(), &input.read.groups, &host),
                },
                empty: messages.is_empty().then(|| "No messages yet".into()),
                messages,
                bubbles: false,
                members: Some(members_panel(c, &input.members, now)),
                composer: composer(can_post, "Read only", name, input.read.pending.clone()),
                signature: String::new(),
            }
        }
        LibraryPlace::Direct { device_id } => {
            // Only while the two are connected directly (a conversation just started from a channel's members is not
            // listed yet: the core says why if it cannot be written to).
            let can_post = input.direct.as_ref().filter(|d| &d.device_id == device_id).is_none_or(|d| d.online);
            let messages = cards(input.read.messages.clone(), &input.read.own_devices, &input.read.joined, can_post, clock.as_ref(), now);
            ChatPageModel {
                place: format!("dm:{device_id}"),
                topic: None,
                toolbar: ChatPageToolbar { tag: None, members: None, share: None, menu: direct_menu(&input.title, input.read.direct_agents, &host) },
                empty: messages.is_empty().then(|| "No messages yet".into()),
                messages,
                bubbles: true,
                members: None,
                // Why, at length, is in the error a send gets (`DIRECT_OFFLINE`); here one short line.
                composer: composer(can_post, &format!("You can write when {} is connected directly", input.title), &input.title, input.read.pending.clone()),
                signature: String::new(),
            }
        }
        _ => return None,
    };
    let mut h = std::collections::hash_map::DefaultHasher::new();
    format!("{m:?}").hash(&mut h);
    m.signature = format!("{:016x}", h.finish());
    Some(m)
}

#[uniffi::export]
impl Core {
    /// What a conversation's page shows of it: its newest messages ([`PAGE_MESSAGES`], oldest first) and its pending
    /// sends, a channel's On All My Devices and the private messages' agent setting. The page is on screen when this
    /// runs: what it shows counts as read (a channel's General, a topic or a private conversation). Cheap; the host
    /// calls it with each refresh of the page (the library's `refresh_ms`) and after a send.
    pub fn chat_page_read(&self, place: LibraryPlace) -> ChatPageRead {
        let sync = self.backend.sync();
        let mut read = ChatPageRead {
            messages: Vec::new(),
            pending: Vec::new(),
            follow: None,
            direct_agents: sync.chat_direct_agents(),
            own_devices: sync.own_devices(),
            joined: Vec::new(),
            groups: sync.chat_groups(),
        };
        match place {
            LibraryPlace::Chat { channel_id, topic_id } => {
                let Some(c) = sync.chat_channels().into_iter().find(|c| c.id == channel_id) else { return read };
                read.messages = sync.chat_messages(&channel_id, topic_id.as_deref(), None, PAGE_MESSAGES);
                let topics = sync.chat_topics(&channel_id);
                let unread = match &topic_id {
                    None => general_unread(&c, &topics),
                    Some(t) => topics.iter().find(|x| &x.id == t).map_or(0, |x| x.unread),
                };
                if unread > 0 {
                    sync.chat_mark_read(&channel_id, topic_id.as_deref());
                }
                read.pending = self.chat_pending_sends(channel_id.clone());
                read.follow = Some(follow_line_for(self.backend.as_ref(), &channel_id));
                read.joined = sync.chat_joined(&channel_id);
            }
            LibraryPlace::Direct { device_id } => {
                read.messages = sync.chat_direct_messages(&device_id, None, PAGE_MESSAGES);
                if sync.chat_directs().iter().any(|d| d.device_id == device_id && d.unread > 0) {
                    sync.chat_mark_direct_read(&device_id);
                }
                read.pending = self.chat_pending_sends(format!("dm:{device_id}"));
            }
            _ => {}
        }
        read
    }
}

// MARK: Samples

/// Complex messages, the way agents and people write them: the Mac's snapshot renders them (`chat-markdown-*`) and the
/// tests parse them, so the Markdown of SPEC 7G.7 is checked on real shapes rather than one-liners.
pub(crate) mod samples {
    pub(crate) const REPORT: &str = "# Build report for 0.1.7

All **283** Rust tests and *292* Swift tests passed.
Details are in `target/nextest/report.xml`.

## Steps

1. Bumped the version in `Cargo.toml`
2. Ran the checks:
   - `tools/check.sh` (clippy and the tests)
   - the Xcode build with Swift 6.3.3
3. Built the public commit

### Next

- [x] CI green
- [ ] Tag `v0.1.7`
- [ ] Publish the draft

> The daylight saving change today moved the times by one hour.
> **Check** the stamps in older logs.

---

```rust
fn time(at: u64) -> String {
    clock(at, offset_at(at)) // local, with the offset of that moment
}
```";

    pub(crate) const TABLE: &str = "## Download page

| Build | Size | SHA-256 | Notes |
|:------|-----:|---------|-------|
| 0.1.6 | 13.4 MB | `113c97b5…` | published |
| 0.1.7 | 13.6 MB | `pending` | **draft** |
| 0.1.5 | 12.9 MB | | old \\| kept |

The table is in `site/download.md`.";

    pub(crate) const NORWEGIAN: &str =
        "Takk! 🙌 Kan dere sjekke **lørdag** før ~~fredag~~? Se [planen](https://example.com/plan) og `~/Shared/Middag.pdf`.";

    pub(crate) const CODE: &str = "The share extension logs one line per use. To see them:

~~~sh
/usr/bin/log show --last 10m --info --style compact \\
  --predicate 'subsystem == \"io.github.kkollsga.Seedwell\" AND category == \"share-menu\"'
# expect: share: not synced | share dialog | may not share
```not a fence end```
~~~

And a long line without a language, which scrolls sideways instead of wrapping:

```
2026-10-25 20:14:03.118 SeedwellShare[76855:1203442] share: not synced for <private> (pid 76855, build 143, the 15:15 release candidate before the published 0.1.6)
```";

    pub(crate) const NESTED: &str = "7. Seventh, numbered as written
8. Eighth
   continued on an indented line
   - a point under it
     - and one deeper
       - and the deepest
9. Ninth with `code`, **bold** and a [link](https://example.com)

#hashtag, 3.5 apples and a lone 1. are plain text.";

    pub(crate) const BUG_MAIL: &str = "---
mail: 1
to: kglite
type: bug
summary: The open-source graph serves the wrong repo after a switch
---
After `repo_management('pydata/xarray')`, `graph_overview` still answers for the previous repo.

## Ask
1. Rebuild the active graph when the repo changes.
2. Tell us the version with the fix.

## References
- `mcp-servers/open_source/server.py:212`";

    pub(crate) const STATUS_MAIL: &str = "---
mail: 1
to: mcp-servers
type: status
re: 7f1b588c
state: done
---
Fixed in **0.17.13**: the graph is rebuilt on every switch. Plan in `dev-docs/plans/open-source.md`; nothing pending.";
}

/// The sample conversation of the snapshots (`chat-markdown-*`): complex messages from agents and people, each by
/// another device a minute apart from 1 Oct 2026, the bug mail tagged by an agent and by you. Never chat state.
#[uniffi::export]
pub fn chat_sample_messages() -> Vec<SyncMessage> {
    use samples::*;
    let list: [(Option<&str>, &str, &str); 8] = [
        (Some("Seedwell"), "Kristian’s Mac mini", REPORT),
        (Some("Website"), "Studio iMac", TABLE),
        (None, "Kari's iPhone", NORWEGIAN),
        (Some("Seedwell"), "Kristian’s MacBook Air", CODE),
        (Some("ci-bot"), "Studio iMac", NESTED),
        (None, "Studio iMac", "Ok"),
        (Some("mcp-servers"), "Kristian’s Mac mini", BUG_MAIL),
        (Some("kglite"), "Studio iMac", STATUS_MAIL),
    ];
    let message = |id: String, device: String, name: &str, at: u64, body: String, me: bool, agent: Option<&str>| SyncMessage {
        id,
        channel_id: "C".into(),
        author_device: device,
        author_name: name.into(),
        author_user: String::new(),
        is_me: me,
        agent: agent.map(str::to_owned),
        at_s: at,
        reply_to: None,
        topic: None,
        kind: SyncMessageKind::Text,
        body,
        images: Vec::new(),
        files: Vec::new(),
        place: "C".into(),
    };
    let start = 1_792_953_675;
    let mut out: Vec<SyncMessage> = list
        .iter()
        .enumerate()
        .map(|(n, (agent, computer, body))| message(format!("5a3e{n:04}c0ffee"), format!("D{n}"), computer, start + n as u64 * 60, (*body).into(), false, *agent))
        .collect();
    let bug = out[6].id.clone();
    let tags = |t: &[&str]| crate::mail::chat_tag_note(&bug, t.iter().map(|s| s.to_string()).collect());
    out.push(message("7a9e0001c0ffee".into(), "D9".into(), "Studio iMac", start + 600, tags(&["seen", "backlog"]), false, Some("kglite")));
    out.push(message("7a9e0002c0ffee".into(), "D10".into(), "Mac mini", start + 660, tags(&["needs-triage"]), true, None));
    out
}

#[cfg(test)]
#[path = "chat_page_tests.rs"]
mod tests;
