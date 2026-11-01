//! The chat dialogs (SPEC 7G.7) as models every host draws (docs/CROSS-PLATFORM.md §1): the New Channel window with its
//! Join with a Link side, and the small dialogs of a channel and a private conversation (New Topic, Rename, Wake
//! Command, Leave, Delete Conversation). Rust gives every text, the buttons (Cancel and the action, bottom right),
//! what the typed text becomes and why it cannot be used; the host draws them, runs the core call of the action and
//! shows a failure under [`ChatDialog::failure_title`] with the error's sentence (`core_error_message`).

use trs_platform::computer;
use trs_sync_proto::keys::KeyError;
use trs_sync_proto::members::InviteLink;

use crate::chat_text::{agents_menu, chat_new_agents_note, ChatAgentsChoice, AGENTS_NO, AGENTS_YES};
use crate::devices_text::FollowLine;
use crate::sync::{readable, SyncChannel, SyncLinkInfo};
use crate::sync_text::{role, sentence};
use crate::types::CoreError;
use crate::Core;

/// The buttons' "Cancel" (left of the action, bottom right).
const CANCEL: &str = "Cancel";

/// The longest channel name kept, in characters.
const NAME_MAX: usize = 64;
/// The longest topic title kept, in characters.
const TOPIC_MAX: usize = 100;

/// Which dialog it is: what its action runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum ChatDialogKind {
    /// `chat_new_topic(channel, title)`, then the topic's page.
    NewTopic,
    /// `chat_rename(channel, name)` (on this computer only).
    RenameChannel,
    /// `chat_set_wake_command(channel, command)` (`None`: no command).
    WakeCommand,
    /// `chat_leave(channel)`.
    LeaveChannel,
    /// `chat_delete_direct(device)`, then the Folder Sync page.
    DeleteConversation,
    /// `chat_set_agents(channel, !channel.agents)`: Agents: Yes/No… of the sidebar's menu, switched the other way.
    ChannelAgents,
}

/// The text field of a dialog that asks for one.
#[derive(Clone, Debug, PartialEq, Eq, Hash, uniffi::Record)]
pub struct ChatDialogField {
    /// What the field is ("Title", "Name", "Command"), for a host that labels its fields (the Mac's alert does not).
    pub label: String,
    /// Grey text while it is empty (empty: none).
    pub placeholder: String,
    /// The text it starts with (the name now, the command now).
    pub text: String,
}

/// A small dialog: a question or a request for one line of text, with Cancel and the action.
#[derive(Clone, Debug, PartialEq, Eq, Hash, uniffi::Record)]
pub struct ChatDialog {
    pub kind: ChatDialogKind,
    pub title: String,
    pub message: String,
    pub field: Option<ChatDialogField>,
    /// "Cancel".
    pub cancel_title: String,
    /// The action's button ("Start", "Rename", "Save", "Leave", "Delete"): the default one, rightmost.
    pub confirm_title: String,
    /// The action removes something: its button is drawn as destructive.
    pub destructive: bool,
    /// The alert's title when the action fails ("Couldn't rename the channel"); its text is the error's sentence.
    pub failure_title: String,
}

/// What the action does with what was typed ([`chat_dialog_check`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum ChatDialogInput {
    /// The action may run, with this text (`None`: a dialog without a field, or an empty wake command: none).
    Ready { value: Option<String> },
    /// Nothing changes (the name it has): the action only closes the dialog.
    Unchanged,
    /// The action cannot run: why, as a sentence. The host disables the action or keeps the dialog open.
    Problem { text: String },
}

fn trimmed(text: &str, max: usize) -> String {
    text.trim().chars().take(max).collect()
}

fn field(label: &str, placeholder: &str, text: &str) -> Option<ChatDialogField> {
    Some(ChatDialogField { label: label.into(), placeholder: placeholder.into(), text: text.into() })
}

fn dialog(kind: ChatDialogKind, title: String, message: String, confirm: &str, failure: String) -> ChatDialog {
    ChatDialog {
        kind,
        title,
        message,
        field: None,
        cancel_title: CANCEL.into(),
        confirm_title: confirm.into(),
        destructive: false,
        failure_title: failure,
    }
}

/// New Topic… of a channel: asks for the topic's title.
#[uniffi::export]
pub fn chat_new_topic_dialog(channel: SyncChannel) -> ChatDialog {
    ChatDialog {
        field: field("Title", "Dinner on Saturday", ""),
        ..dialog(
            ChatDialogKind::NewTopic,
            format!("New topic in “{}”", channel.name),
            "A topic keeps a conversation apart from the channel’s General. Everyone in the channel sees it.".into(),
            "Start",
            "Couldn't start the topic".into(),
        )
    }
}

/// Rename… of a channel: asks for its new name, on this computer only (each device names its channels, as folders).
#[uniffi::export]
pub fn chat_rename_dialog(channel: SyncChannel) -> ChatDialog {
    ChatDialog {
        field: field("Name", "", &channel.name),
        ..dialog(
            ChatDialogKind::RenameChannel,
            format!("Rename “{}”", channel.name),
            concat!("The new name is used on this ", computer!(), ". The other devices keep the name they know it by.").into(),
            "Rename",
            "Couldn't rename the channel".into(),
        )
    }
}

/// Wake Command… of a channel (only while it is open to agents): the command that wakes an agent when a message
/// arrives; empty for none. It gets the message as JSON on its standard input, never as arguments.
#[uniffi::export]
pub fn chat_wake_command_dialog(channel: SyncChannel) -> ChatDialog {
    ChatDialog {
        field: field("Command", "~/bin/wake-agent", channel.wake_command.as_deref().unwrap_or_default()),
        ..dialog(
            ChatDialogKind::WakeCommand,
            format!("Wake an agent for “{}”", channel.name),
            "This command runs when a message from another device arrives, with the message as JSON on its standard input. It runs only while the channel is open to agents (Agents: Yes). Leave it empty for none.".into(),
            "Save",
            "Couldn't set the command".into(),
        )
    }
}

/// Leave Channel…: the question. The other members keep the channel.
#[uniffi::export]
pub fn chat_leave_dialog(channel: SyncChannel) -> ChatDialog {
    let message = if channel.role == crate::sync::SyncRole::Owner {
        concat!("Its messages are removed from this ", computer!(), ". The other members keep the channel; make another device an owner first if it should keep one.")
    } else {
        concat!("Its messages are removed from this ", computer!(), ". The other members keep the channel.")
    };
    ChatDialog {
        destructive: true,
        ..dialog(
            ChatDialogKind::LeaveChannel,
            format!("Leave “{}”?", channel.name),
            message.into(),
            "Leave",
            format!("Couldn't leave “{}”", channel.name),
        )
    }
}

/// Delete Conversation… of a private conversation with `name` (the device as the page names it): the question. The
/// other device keeps its copy.
#[uniffi::export]
pub fn chat_delete_conversation_dialog(name: String) -> ChatDialog {
    ChatDialog {
        destructive: true,
        ..dialog(
            ChatDialogKind::DeleteConversation,
            format!("Delete the conversation with {name}?"),
            format!(concat!("Its messages are removed from this ", computer!(), ", with everything this ", computer!(), " knows of that device through it. {} keeps its copy."), name),
            "Delete",
            "Couldn't delete the conversation".into(),
        )
    }
}

/// Agents: Yes/No… of a channel (owners and admins): the question of switching it the other way, saying what that
/// means.
#[uniffi::export]
pub fn chat_agents_dialog(channel: SyncChannel) -> ChatDialog {
    let (title, confirm, means) = if channel.agents {
        (format!("Close “{}” to agents?", channel.name), "Close to Agents", AGENTS_NO)
    } else {
        (format!("Open “{}” to agents?", channel.name), "Open to Agents", AGENTS_YES)
    };
    let message = format!("{means} This is one setting for the whole channel, on every member's device.");
    dialog(ChatDialogKind::ChannelAgents, title, message, confirm, agents_menu(&channel).failure_title)
}

/// What the action of `dialog` does with `input`, the field's text as typed: a topic title or a channel name trimmed
/// and cut to its length (100, 64 characters), never empty; a wake command trimmed, empty for none; the name the
/// channel has already changes nothing.
#[uniffi::export]
pub fn chat_dialog_check(dialog: ChatDialog, input: String) -> ChatDialogInput {
    let unchanged = |v: &str| dialog.field.as_ref().is_some_and(|f| f.text.trim() == v);
    match dialog.kind {
        ChatDialogKind::NewTopic => match trimmed(&input, TOPIC_MAX) {
            t if t.is_empty() => ChatDialogInput::Problem { text: "Give the topic a title.".into() },
            t => ChatDialogInput::Ready { value: Some(t) },
        },
        ChatDialogKind::RenameChannel => match trimmed(&input, NAME_MAX) {
            n if n.is_empty() => ChatDialogInput::Problem { text: NAME_PROBLEM.into() },
            n if unchanged(&n) => ChatDialogInput::Unchanged,
            n => ChatDialogInput::Ready { value: Some(n) },
        },
        ChatDialogKind::WakeCommand => match input.trim() {
            c if unchanged(c) => ChatDialogInput::Unchanged,
            "" => ChatDialogInput::Ready { value: None },
            c => ChatDialogInput::Ready { value: Some(c.to_owned()) },
        },
        ChatDialogKind::LeaveChannel | ChatDialogKind::DeleteConversation | ChatDialogKind::ChannelAgents => ChatDialogInput::Ready { value: None },
    }
}

const NAME_PROBLEM: &str = "Give the channel a name.";

// MARK: New Channel and Join with a Link

/// The New Channel window's two sides.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum ChatNewMode {
    /// Make a channel (this computer becomes its owner).
    New,
    /// Join one with a `seedwell://chat/` link.
    Join,
}

/// One side in the window's switch.
#[derive(Clone, Debug, PartialEq, Eq, Hash, uniffi::Record)]
pub struct ChatNewModeChoice {
    pub mode: ChatNewMode,
    /// "New Channel", "Join with a Link".
    pub title: String,
}

/// What is chosen and typed in the New Channel window. The host keeps it as a value, edits `mode`, `name`, `agents` and
/// `all_devices` itself and the link through [`Core::chat_new_set_link`].
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ChatNewForm {
    pub mode: ChatNewMode,
    /// The new channel's name as typed.
    pub name: String,
    /// Agents: Yes for the new channel; No unless the user picks Yes.
    pub agents: bool,
    /// "On all my devices" as it was when the window opened (shown only while this computer is paired with others).
    pub follow: FollowLine,
    /// That toggle for the new channel: on by default while it is shown.
    pub all_devices: bool,
    /// The link as typed, what it is and why it cannot be used (the core's words; none when it is a link or empty).
    pub link: String,
    pub info: Option<SyncLinkInfo>,
    pub problem: Option<String>,
}

/// The New Channel window as drawn from a [`ChatNewForm`].
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ChatNewWindowModel {
    /// The small caption over the title: "Chats".
    pub caption: String,
    /// "New Channel" or "Join a Channel".
    pub title: String,
    /// The switch between the sides, in order.
    pub modes: Vec<ChatNewModeChoice>,
    // New Channel
    pub name_label: String,
    pub name_placeholder: String,
    /// The line under the name ("You become its owner…").
    pub name_note: String,
    pub agents_label: String,
    /// Yes, then No, the form's checked.
    pub agents_choices: Vec<ChatAgentsChoice>,
    /// What the chosen one means.
    pub agents_note: String,
    /// "On all my devices", while it is shown; `on` is the form's choice.
    pub follow: Option<FollowLine>,
    // Join with a Link
    pub link_label: String,
    pub link_placeholder: String,
    /// The line under the link: what it is ("Read & write · Family") or why it cannot be joined. Empty while nothing
    /// is pasted.
    pub link_line: String,
    /// That line is about a link that can be joined (drawn with a check mark; else as a warning).
    pub link_ok: bool,
    // Buttons
    pub cancel_title: String,
    /// "Start".
    pub confirm_title: String,
    /// "Make the channel", "Join the channel".
    pub confirm_help: String,
    pub can_start: bool,
    /// The alert's or the window's error title when Start fails.
    pub failure_title: String,
}

/// A text worth prefilling the Join side with (from the pasteboard or the clipboard): a `seedwell://chat/` link.
#[uniffi::export]
pub fn chat_looks_like_link(text: String) -> bool {
    let t = text.trim();
    t.chars().count() < 2_000 && t.to_lowercase().starts_with("seedwell://chat/")
}

/// The line under the Join side's link: "Read & write · Family", or why it cannot be joined.
fn chat_link_line(text: &str, info: Option<&SyncLinkInfo>, problem: Option<&str>) -> String {
    if text.trim().is_empty() {
        return String::new();
    }
    let Some(info) = info else {
        return problem.map_or_else(|| "Not a Seedwell channel link".into(), sentence);
    };
    if info.already_synced {
        return concat!("This channel is already on this ", computer!()).into();
    }
    if info.expired {
        return "This link has expired: ask for a new one".into();
    }
    [Some(role(info.role).to_string()), info.name.clone()].into_iter().flatten().collect::<Vec<_>>().join(" · ")
}

/// Why `text` is not a channel link, in the channel's words; `None` when it is one, when it is no Seedwell link at all
/// (the line says so) or empty.
fn chat_link_problem(text: &str) -> Option<String> {
    if text.trim().is_empty() {
        return None;
    }
    match InviteLink::parse_chat(text).err()? {
        KeyError::Link | KeyError::OldKey => None,
        KeyError::Damaged => Some("The channel link is damaged or incomplete.".into()),
        KeyError::Name => Some("The channel name in the link is damaged.".into()),
        e => Some(readable(&e.to_string())),
    }
}

fn link_usable(form: &ChatNewForm) -> bool {
    form.info.as_ref().is_some_and(|i| !i.expired && !i.already_synced)
}

fn can_start(form: &ChatNewForm) -> bool {
    match form.mode {
        ChatNewMode::New => !trimmed(&form.name, NAME_MAX).is_empty(),
        ChatNewMode::Join => link_usable(form),
    }
}

/// The form with `text` as its link, `info` what the core read in it.
fn with_link(form: ChatNewForm, text: String, info: Option<SyncLinkInfo>) -> ChatNewForm {
    let problem = if info.is_none() { chat_link_problem(&text) } else { None };
    ChatNewForm { link: text, info, problem, ..form }
}

/// A fresh form: New Channel, Agents: No, "On all my devices" as `follow` has it.
fn new_form(follow: FollowLine) -> ChatNewForm {
    ChatNewForm {
        mode: ChatNewMode::New,
        name: String::new(),
        agents: false,
        all_devices: follow.shown && follow.on,
        follow,
        link: String::new(),
        info: None,
        problem: None,
    }
}

/// The New Channel window drawn from `form`.
#[uniffi::export]
pub fn chat_new_window(form: ChatNewForm) -> ChatNewWindowModel {
    let new = form.mode == ChatNewMode::New;
    ChatNewWindowModel {
        caption: "Chats".into(),
        title: if new { "New Channel" } else { "Join a Channel" }.into(),
        modes: [(ChatNewMode::New, "New Channel"), (ChatNewMode::Join, "Join with a Link")]
            .map(|(mode, t)| ChatNewModeChoice { mode, title: t.into() })
            .to_vec(),
        name_label: "Name".into(),
        name_placeholder: "Family, Studio, Agents…".into(),
        name_note: concat!("You become its owner. After Start, Share… makes a link for each ", computer!(), " that should join.").into(),
        agents_label: "Agents".into(),
        agents_choices: [("Yes", true), ("No", false)].map(|(t, open)| ChatAgentsChoice { title: t.into(), open, checked: form.agents == open }).to_vec(),
        agents_note: chat_new_agents_note(form.agents),
        follow: form.follow.shown.then(|| FollowLine { on: form.all_devices, ..form.follow.clone() }),
        link_label: "Link".into(),
        link_placeholder: "seedwell://chat/…".into(),
        link_line: chat_link_line(&form.link, form.info.as_ref(), form.problem.as_deref()),
        link_ok: link_usable(&form),
        cancel_title: CANCEL.into(),
        confirm_title: "Start".into(),
        confirm_help: if new { "Make the channel" } else { "Join the channel" }.into(),
        can_start: can_start(&form),
        failure_title: if new { "Couldn't make the channel" } else { "Couldn't join the channel" }.into(),
    }
}

#[uniffi::export]
impl Core {
    /// The New Channel window's form as it opens: New Channel, or Join with `prefill` (a link that opened the app, or
    /// the pasteboard's when [`chat_looks_like_link`]) read already.
    pub fn chat_new_form(&self, prefill: Option<String>) -> ChatNewForm {
        let form = new_form(self.sync_follow_line_new(true));
        match prefill {
            Some(text) => ChatNewForm { mode: ChatNewMode::Join, ..self.chat_new_set_link(form, text) },
            None => form,
        }
    }

    /// The form with `text` typed or pasted as its link: read by the core (what it is, or why it is not a link this
    /// computer can join).
    pub fn chat_new_set_link(&self, form: ChatNewForm, text: String) -> ChatNewForm {
        let t = text.trim();
        let info = if t.is_empty() { None } else { self.chat_parse_link(t.to_owned()) };
        with_link(form, text, info)
    }

    /// Start: makes the channel (named as typed, trimmed, Agents as chosen; on all the user's devices when that is
    /// chosen and shown) or joins the linked one. Slow (the network): call it off the main thread. Then the host shows
    /// the channel, and for a new one its Share dialog: the next step is sharing it.
    pub fn chat_new_start(&self, form: ChatNewForm) -> Result<SyncChannel, CoreError> {
        let view = chat_new_window(form.clone());
        if !view.can_start {
            let message = match form.mode {
                ChatNewMode::New => NAME_PROBLEM.to_owned(),
                ChatNewMode::Join if form.link.trim().is_empty() => "Paste a link to the channel.".to_owned(),
                ChatNewMode::Join => readable(&view.link_line),
            };
            return Err(CoreError::Failed { message });
        }
        match form.mode {
            ChatNewMode::New => {
                let c = self.chat_new(trimmed(&form.name, NAME_MAX), form.agents)?;
                // On all my devices: the others join it by themselves. The channel is made either way.
                if form.follow.shown && form.all_devices {
                    let _ = self.sync_set_follow(c.id.clone(), true);
                }
                Ok(c)
            }
            ChatNewMode::Join => self.chat_join(form.link.trim().to_owned()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat_text::tests::channel;
    use crate::sync::{parse_chat_link as read_link, SyncRole};

    fn follow(shown: bool, on: bool) -> FollowLine {
        FollowLine {
            shown,
            title: "On all my devices".into(),
            menu_title: "On All My Devices".into(),
            on,
            enabled: true,
            line: "Your other devices join it by themselves.".into(),
            help: String::new(),
        }
    }

    #[test]
    fn the_small_dialogs_have_their_texts_and_cancel_and_the_action() {
        let mut c = channel("Team", 2, 1, 0, false);
        c.wake_command = Some("~/bin/wake".into());
        let topic = chat_new_topic_dialog(c.clone());
        assert_eq!((topic.title.as_str(), topic.confirm_title.as_str(), topic.cancel_title.as_str()), ("New topic in “Team”", "Start", "Cancel"));
        assert_eq!(topic.field.as_ref().map(|f| (f.placeholder.as_str(), f.text.as_str())), Some(("Dinner on Saturday", "")));
        assert_eq!(topic.failure_title, "Couldn't start the topic");
        let rename = chat_rename_dialog(c.clone());
        assert_eq!((rename.title.as_str(), rename.confirm_title.as_str()), ("Rename “Team”", "Rename"));
        assert_eq!(rename.field.as_ref().map(|f| f.text.as_str()), Some("Team"), "the field starts with the name");
        assert!(rename.message.starts_with(concat!("The new name is used on this ", computer!(), ".")), "{}", rename.message);
        let wake = chat_wake_command_dialog(c.clone());
        assert_eq!((wake.title.as_str(), wake.confirm_title.as_str()), ("Wake an agent for “Team”", "Save"));
        assert_eq!(wake.field.as_ref().map(|f| (f.placeholder.as_str(), f.text.as_str())), Some(("~/bin/wake-agent", "~/bin/wake")));
        assert!(wake.message.contains("JSON on its standard input") && wake.message.ends_with("Leave it empty for none."));
        for d in [&topic, &rename, &wake] {
            assert!(!d.destructive && d.field.is_some(), "{:?}", d.kind);
        }
        let leave = chat_leave_dialog(c.clone());
        assert_eq!((leave.title.as_str(), leave.confirm_title.as_str(), leave.destructive, leave.field.is_none()), ("Leave “Team”?", "Leave", true, true));
        assert!(leave.message.contains("make another device an owner first"), "an owner is told: {}", leave.message);
        assert_eq!(leave.failure_title, "Couldn't leave “Team”");
        c.role = SyncRole::ReadWrite;
        assert!(!chat_leave_dialog(c).message.contains("owner"));
        let delete = chat_delete_conversation_dialog("Kari (Studio iMac)".into());
        assert_eq!((delete.title.as_str(), delete.confirm_title.as_str(), delete.destructive), ("Delete the conversation with Kari (Studio iMac)?", "Delete", true));
        assert!(delete.message.ends_with("Kari (Studio iMac) keeps its copy."), "{}", delete.message);
        assert_eq!(delete.failure_title, "Couldn't delete the conversation");
    }

    #[test]
    fn the_agents_question_switches_the_channel_the_other_way() {
        let mut c = channel("Bridge", 2, 1, 0, false);
        let open = chat_agents_dialog(c.clone());
        assert_eq!((open.title.as_str(), open.confirm_title.as_str(), open.field.is_none()), ("Open “Bridge” to agents?", "Open to Agents", true));
        assert!(open.message.starts_with(AGENTS_YES), "{}", open.message);
        assert_eq!(open.failure_title, "Couldn't change whether agents may use “Bridge”");
        c.agents = true;
        let close = chat_agents_dialog(c);
        assert_eq!((close.title.as_str(), close.confirm_title.as_str()), ("Close “Bridge” to agents?", "Close to Agents"));
        assert!(close.message.starts_with(AGENTS_NO));
        assert_eq!(chat_dialog_check(close, String::new()), ChatDialogInput::Ready { value: None });
    }

    #[test]
    fn what_is_typed_is_checked_trimmed_and_cut_to_its_length() {
        let c = channel("Team", 2, 1, 0, false);
        let topic = chat_new_topic_dialog(c.clone());
        let check = |d: &ChatDialog, s: &str| chat_dialog_check(d.clone(), s.into());
        assert_eq!(check(&topic, "  \n "), ChatDialogInput::Problem { text: "Give the topic a title.".into() });
        assert_eq!(check(&topic, "  Dinner \n"), ChatDialogInput::Ready { value: Some("Dinner".into()) });
        assert_eq!(check(&topic, &"é".repeat(150)), ChatDialogInput::Ready { value: Some("é".repeat(100)) }, "100 characters, not bytes");
        let rename = chat_rename_dialog(c.clone());
        assert_eq!(check(&rename, " "), ChatDialogInput::Problem { text: "Give the channel a name.".into() });
        assert_eq!(check(&rename, " Team "), ChatDialogInput::Unchanged, "the name it has");
        assert_eq!(check(&rename, "Family"), ChatDialogInput::Ready { value: Some("Family".into()) });
        assert_eq!(check(&rename, &"x".repeat(70)), ChatDialogInput::Ready { value: Some("x".repeat(64)) });
        let wake = chat_wake_command_dialog(c.clone());
        assert_eq!(check(&wake, "  "), ChatDialogInput::Unchanged, "no command, and none before");
        assert_eq!(check(&wake, " ~/bin/wake "), ChatDialogInput::Ready { value: Some("~/bin/wake".into()) });
        let had = chat_wake_command_dialog(SyncChannel { wake_command: Some("~/bin/wake".into()), ..c.clone() });
        assert_eq!(check(&had, ""), ChatDialogInput::Ready { value: None }, "emptied: no command");
        assert_eq!(check(&had, "~/bin/wake"), ChatDialogInput::Unchanged);
        assert_eq!(check(&chat_leave_dialog(c), "whatever"), ChatDialogInput::Ready { value: None });
    }

    #[test]
    fn the_new_channel_side_needs_a_name_and_says_what_agents_mean() {
        let form = new_form(follow(true, true));
        assert!(form.all_devices, "on all my devices by default while shown");
        let v = chat_new_window(form.clone());
        assert_eq!((v.caption.as_str(), v.title.as_str(), v.confirm_title.as_str(), v.cancel_title.as_str()), ("Chats", "New Channel", "Start", "Cancel"));
        assert_eq!(v.modes.iter().map(|m| m.title.as_str()).collect::<Vec<_>>(), ["New Channel", "Join with a Link"]);
        assert_eq!((v.name_label.as_str(), v.name_placeholder.as_str()), ("Name", "Family, Studio, Agents…"));
        assert!(v.name_note.starts_with("You become its owner."));
        assert_eq!(v.agents_choices.iter().map(|c| (c.title.as_str(), c.checked)).collect::<Vec<_>>(), [("Yes", false), ("No", true)]);
        assert_eq!(v.agents_note, chat_new_agents_note(false));
        assert_eq!(v.follow.as_ref().map(|f| f.on), Some(true));
        assert!(!v.can_start, "no name yet");
        assert_eq!(v.confirm_help, "Make the channel");
        let named = ChatNewForm { name: "  Family ".into(), agents: true, all_devices: false, ..form };
        let v = chat_new_window(named);
        assert!(v.can_start);
        assert_eq!(v.agents_note, chat_new_agents_note(true));
        assert_eq!(v.follow.map(|f| f.on), Some(false), "the user's choice");
        assert_eq!(chat_new_window(new_form(follow(false, true))).follow, None, "not paired: no toggle");
        assert!(!new_form(follow(false, true)).all_devices);
    }

    #[test]
    fn the_join_side_reads_the_link_and_says_why_it_cannot_be_joined() {
        let join = |text: &str| {
            let f = ChatNewForm { mode: ChatNewMode::Join, ..new_form(follow(false, false)) };
            chat_new_window(with_link(f, text.into(), read_link(text)))
        };
        let v = join("");
        assert_eq!((v.title.as_str(), v.link_line.as_str(), v.can_start, v.confirm_help.as_str()), ("Join a Channel", "", false, "Join the channel"));
        assert_eq!((v.link_label.as_str(), v.link_placeholder.as_str()), ("Link", "seedwell://chat/…"));
        let v = join("hello");
        assert_eq!((v.link_line.as_str(), v.link_ok), ("Not a Seedwell channel link", false));
        assert_eq!(join("seedwell://chat/AAAA").link_line, "The channel link is damaged or incomplete");
        let info = |already: bool, expired: bool| SyncLinkInfo {
            role: SyncRole::ReadWrite,
            name: Some("Family".into()),
            folder_id: "c".into(),
            already_synced: already,
            expires_s: None,
            expired,
            channel_name: None,
            channel_joined: false,
        };
        let line = |i: SyncLinkInfo| {
            let f = ChatNewForm { mode: ChatNewMode::Join, ..new_form(follow(false, false)) };
            chat_new_window(with_link(f, "seedwell://chat/x".into(), Some(i)))
        };
        let ok = line(info(false, false));
        assert_eq!((ok.link_line.as_str(), ok.link_ok, ok.can_start), ("Read & write · Family", true, true));
        let here = line(info(true, false));
        assert_eq!((here.link_line.as_str(), here.can_start), (concat!("This channel is already on this ", computer!()), false));
        let old = line(info(false, true));
        assert_eq!((old.link_line.as_str(), old.can_start), ("This link has expired: ask for a new one", false));
        assert!(chat_looks_like_link(" SEEDWELL://chat/abc ".into()) && !chat_looks_like_link("seedwell://sync/abc".into()));
        assert!(!chat_looks_like_link(format!("seedwell://chat/{}", "a".repeat(2_000))));
    }

    #[test]
    fn the_core_makes_or_joins_the_channel_and_refuses_what_cannot_start() {
        let (core, _d) = crate::library::tests::mock();
        let form = core.chat_new_form(None);
        assert_eq!(form.mode, ChatNewMode::New);
        let e = core.chat_new_start(form.clone()).unwrap_err();
        assert_eq!(crate::sync_text::core_error_message(e), "Give the channel a name.");
        let c = core.chat_new_start(ChatNewForm { name: "  Dinner club ".into(), agents: true, ..form.clone() }).unwrap();
        assert_eq!((c.name.as_str(), c.agents), ("Dinner club", true));
        let link = core.sync_create_invite(c.id.clone(), SyncRole::ReadWrite, 1, 7, None).unwrap().link;
        let prefilled = core.chat_new_form(Some(link.clone()));
        assert_eq!(prefilled.mode, ChatNewMode::Join);
        let v = chat_new_window(prefilled.clone());
        assert_eq!((v.link_line.as_str(), v.can_start), ("Read & write · Dinner club", true));
        let joined = core.chat_new_start(prefilled.clone()).unwrap();
        // The mock's links name channels of their own; a link to one this computer has cannot be joined again.
        let mut again = prefilled;
        again.info.as_mut().unwrap().already_synced = true;
        assert!(!chat_new_window(again.clone()).can_start, "{joined:?}");
        let e = core.chat_new_start(again).unwrap_err();
        assert_eq!(crate::sync_text::core_error_message(e), concat!("This channel is already on this ", computer!(), "."));
        let typed = core.chat_new_set_link(form.clone(), "nonsense".into());
        assert_eq!((typed.link.as_str(), typed.info.is_none()), ("nonsense", true));
        assert_eq!(chat_new_window(typed).link_line, "Not a Seedwell channel link");
        let e = core.chat_new_start(ChatNewForm { mode: ChatNewMode::Join, ..form.clone() }).unwrap_err();
        assert_eq!(crate::sync_text::core_error_message(e), "Paste a link to the channel.");
        // A folder's link pasted here says where it belongs.
        let notes = core.sync_add_folder("/tmp/seedwell-mock/Notes".into()).unwrap();
        let folder_link = core.sync_create_invite(notes.id, SyncRole::ReadWrite, 1, 7, None).unwrap().link;
        let pasted = core.chat_new_set_link(form, folder_link);
        assert_eq!(chat_new_window(pasted).link_line, "This is a folder link: add it under Folder Sync");
    }
}
