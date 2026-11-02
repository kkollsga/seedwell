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

use trs_platform::computer;

use crate::chat_dialogs::{chat_delete_conversation_dialog, chat_leave_dialog, chat_new_topic_dialog, chat_rename_dialog, chat_wake_command_dialog, ChatDialog};
use crate::chat_files::ChatPendingSend;
use crate::chat_images::ChatImage;
use crate::chat_markdown::{blocks, plain, ChatBlock};
use crate::chat_text::{agent_tag, agents_menu, chat_agents_label, general_unread, named, UNNAMED_DEVICE};
use crate::devices_text::{follow_line_for, FollowLine};
use crate::host::{has, HostWindow};
use crate::library::{local_day, local_day_number, LibraryPlace, LocalClock};
use crate::mail::{chat_cards, chat_mail, chat_tag_label, ChatMail, ChatTagOwner};
use crate::sidebar::SidebarTag;
use crate::sync::{SyncAgentAccess, SyncChannel, SyncDirect, SyncMember, SyncMessage, SyncMessageKind, SyncRole, SyncTopic, DIRECT_OFFLINE};
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

/// Who wrote a message, at the start of a run: a person by their name with the device (or the device alone; "You" on
/// this computer), an agent by its name with a badge and the computer it runs on.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ChatMessageAuthor {
    /// Semibold.
    pub name: String,
    /// The "agent" badge (a capsule, like the channel's tag), with its tooltip.
    pub badge: Option<SidebarTag>,
    /// The computer, quiet, after a computer symbol.
    pub computer: Option<String>,
    /// "14.32" today, "Yesterday 14.32", "1 Nov 14.32" before.
    pub time: String,
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

/// A message as a card (yours tinted with the accent colour). A run's first card stands apart from the one before
/// (10 points on the Mac); the rest of the run sits close (4).
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct ChatMessageCard {
    pub id: String,
    /// What its attachments are asked for by (`Core::chat_image_*`, `Core::chat_file_*`): the channel's id, or
    /// `dm:<device>`.
    pub place: String,
    /// At the start of a run only.
    pub author: Option<ChatMessageAuthor>,
    /// The message it answers: "↳ Kari (Studio iMac): Middag på lørdag?", one quiet line.
    pub reply_quote: Option<String>,
    pub body: ChatMessageBody,
    /// Its images and files, after the text: the image view (`chat_images`) and file cards (`chat_files`) as they are.
    pub images: Vec<ChatImage>,
    pub files: Vec<crate::chat_files::ChatFileCard>,
    /// Its tags by owner, on one line under it.
    pub tags: Vec<ChatTagLine>,
    /// Your tags (a tag symbol, shown while the pointer is on the card); `None` where you cannot post.
    pub tag_menu: Option<ChatPageMenu>,
    /// Reply (an arrow symbol, shown while the pointer is on the card); `None` where you cannot post.
    pub reply: Option<ChatPageButton>,
    /// The composer's line while it replies to this message: "Replying to Kari (Studio iMac): Middag på lørdag?".
    pub replying: String,
    /// Yours (not an agent's on this computer): tinted with the accent colour.
    pub mine: bool,
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
    /// It may post; else `cannot_post` shows instead of the field (quiet, 12 points).
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
    /// The tooltip of the reply line's close button.
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

/// A run of messages from one author within five minutes shows the author once; an agent's message and a reply
/// start their own.
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

/// A channel's ⋯ menu: Mute, Agents: Yes/No, On All My Devices, Wake Command…; New Topic…, Rename…; Leave Channel….
fn channel_menu(c: &SyncChannel, follow: Option<&FollowLine>, host: &[HostWindow]) -> ChatPageMenu {
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
    v.push(ChatPageMenuEntry::Separator);
    v.extend(asking("Leave Channel…", ChatPageCommand::Leave, chat_leave_dialog(c.clone()), true, host));
    ChatPageMenu { title: String::new(), help: "Mute, whether agents may use this channel, the wake command, a new topic, rename, leave".into(), entries: tidy(v) }
}

/// A private conversation's ⋯ menu: what agents on this computer may do with private messages; Delete Conversation….
fn direct_menu(name: &str, access: SyncAgentAccess, host: &[HostWindow]) -> ChatPageMenu {
    let levels = [SyncAgentAccess::Off, SyncAgentAccess::Read, SyncAgentAccess::ReadPost]
        .into_iter()
        .map(|a| it(chat_agents_label(a), ChatPageCommand::SetDirectAgents { access: a }).checked(a == access).entry())
        .collect();
    let mut v = vec![ChatPageMenuEntry::Submenu { title: concat!("Agents on This ", computer!()).into(), entries: levels }, ChatPageMenuEntry::Separator];
    v.extend(asking("Delete Conversation…", ChatPageCommand::DeleteConversation, chat_delete_conversation_dialog(name.to_owned()), true, host));
    ChatPageMenu {
        title: String::new(),
        help: concat!("What agents on this ", computer!(), " may do with private messages, and deleting this conversation").into(),
        entries: tidy(v),
    }
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

fn body(m: &SyncMessage) -> ChatMessageBody {
    match m.kind {
        SyncMessageKind::Text => match chat_mail(&m.body) {
            Some(mail) => ChatMessageBody::Mail { banner: banner(&mail), blocks: blocks(&mail.body) },
            None => ChatMessageBody::Markdown { blocks: blocks(&m.body) },
        },
        SyncMessageKind::Json => ChatMessageBody::Json { text: m.body.clone() },
        SyncMessageKind::Other => ChatMessageBody::Unsupported { text: preview(m, 120) },
    }
}

fn message_author(m: &SyncMessage, clock: &dyn LocalClock, now: u64) -> ChatMessageAuthor {
    let time = message_time(clock, m.at_s, now);
    let computer = computer_name(m).to_owned();
    match &m.agent {
        Some(agent) => ChatMessageAuthor {
            name: agent.clone(),
            badge: Some(SidebarTag { text: "agent".into(), help: format!("Sent by an agent through Seedwell on {computer}") }),
            computer: Some(computer),
            time,
        },
        None if !m.is_me && !m.author_user.is_empty() => ChatMessageAuthor { name: m.author_user.clone(), badge: None, computer: Some(computer), time },
        None => ChatMessageAuthor { name: if m.is_me { "You".into() } else { computer }, badge: None, computer: None, time },
    }
}

/// Your tags on a message: the usual ones to switch on and off, and clearing them.
fn tag_menu(id: &str, owners: &[ChatTagOwner]) -> ChatPageMenu {
    let yours: Vec<String> = owners.iter().find(|o| o.is_you).map(|o| o.tags.clone()).unwrap_or_default();
    let set = |tags: Vec<String>| ChatPageCommand::SetTags { message_id: id.to_owned(), tags };
    let mut entries: Vec<ChatPageMenuEntry> = trs_control::mail::KNOWN
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
    ChatPageMenu { title: String::new(), help: "Tag it (your tags; others see them under your name)".into(), entries }
}

/// The messages as cards: tag notes are lines on the message they are about, not cards.
pub(crate) fn cards(messages: Vec<SyncMessage>, can_post: bool, clock: &dyn LocalClock, now: u64) -> Vec<ChatMessageCard> {
    let c = chat_cards(messages);
    let tags: HashMap<String, Vec<ChatTagOwner>> = c.tags.into_iter().map(|t| (t.message, t.owners)).collect();
    let ms = c.cards;
    ms.iter()
        .enumerate()
        .map(|(n, m)| {
            let owners = tags.get(&m.id.chars().take(8).collect::<String>().to_lowercase()).map(Vec::as_slice).unwrap_or_default();
            ChatMessageCard {
                id: m.id.clone(),
                place: m.place.clone(),
                author: starts_run(m, n.checked_sub(1).map(|p| &ms[p])).then(|| message_author(m, clock, now)),
                reply_quote: m.reply_to.as_ref().and_then(|r| ms.iter().find(|x| &x.id == r)).map(|r| format!("↳ {}: {}", author(r), preview(r, 80))),
                body: body(m),
                images: m.images.clone(),
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
                replying: format!("Replying to {}: {}", author(m), preview(m, 60)),
                mine: m.is_me && m.agent.is_none(),
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
            let role_menu = (c.role == SyncRole::Owner && !m.is_me).then(|| {
                let mut entries: Vec<ChatPageMenuEntry> = ROLES
                    .iter()
                    .map(|&r| it(role(r), ChatPageCommand::SetRole { device_id: m.device_id.clone(), role: r }).enabled(r != m.role).entry())
                    .collect();
                entries.push(ChatPageMenuEntry::Separator);
                entries.push(item("Remove from Channel", ChatPageCommand::RemoveDevice { device_id: m.device_id.clone() }));
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
        reply_cancel_help: "Don't reply to it".into(),
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
            let messages = cards(input.read.messages.clone(), can_post, clock.as_ref(), now);
            ChatPageModel {
                place: c.id.clone(),
                topic: topic_id.clone(),
                toolbar: ChatPageToolbar {
                    tag: agent_tag(c),
                    members: Some(button("Members", "Who is in this channel", ChatPageCommand::ShowMembers)),
                    share: (sync_can_share(c.role) && has(&host, Some(HostWindow::ChannelShare)))
                        .then(|| button("Share…", concat!("Make a link that lets another ", computer!(), " join this channel"), ChatPageCommand::Share)),
                    menu: channel_menu(c, input.read.follow.as_ref(), &host),
                },
                empty: messages.is_empty().then(|| "No messages yet.".into()),
                messages,
                members: Some(members_panel(c, &input.members, now)),
                composer: composer(can_post, "Read only: you see the messages but cannot post.", name, input.read.pending.clone()),
                signature: String::new(),
            }
        }
        LibraryPlace::Direct { device_id } => {
            // Only while the two are connected directly (a conversation just started from a channel's members is not
            // listed yet: the core says why if it cannot be written to).
            let can_post = input.direct.as_ref().filter(|d| &d.device_id == device_id).is_none_or(|d| d.online);
            let messages = cards(input.read.messages.clone(), can_post, clock.as_ref(), now);
            ChatPageModel {
                place: format!("dm:{device_id}"),
                topic: None,
                toolbar: ChatPageToolbar { tag: None, members: None, share: None, menu: direct_menu(&input.title, input.read.direct_agents, &host) },
                empty: messages.is_empty().then(|| format!("No messages yet. Only you and {} can read what you write here.", input.title)),
                messages,
                members: None,
                composer: composer(can_post, DIRECT_OFFLINE, &input.title, input.read.pending.clone()),
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
        let mut read = ChatPageRead { messages: Vec::new(), pending: Vec::new(), follow: None, direct_agents: sync.chat_direct_agents() };
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
