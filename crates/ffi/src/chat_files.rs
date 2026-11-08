//! Files in chat for the hosts (SPEC §7G.7 *Files*): each file of a message is a card ([`ChatFileCard`]) with its
//! icon, name, size, state, the line under it and the buttons it offers, all decided here; the host draws it and calls
//! back. Nothing downloads by itself and nothing opens by itself: *Open* on an app or a script asks first
//! ([`Core::chat_file_open`]).
//!
//! * Sending: [`Core::chat_send_attachments`] posts images and files to a channel or a private conversation without
//!   waiting for the files to be read; meanwhile [`Core::chat_pending_sends`] lists them ("Preparing…").
//! * Actions: [`Core::chat_file_download`] (Download, Save As…, Retry), [`Core::chat_file_cancel`],
//!   [`Core::chat_file_stop_sharing`], [`Core::chat_file_open`], [`Core::chat_file_save_name`].

use std::path::{Path, PathBuf};

use seedwell_sync_net::chat::{AttachmentInfo, NewAttachment, NewImage};
use seedwell_sync_net::files::DownloadTo;
use seedwell_sync_net::room::Place;

use crate::Core;
use crate::chat_images::ChatImageInput;
use crate::types::CoreError;

/// What a file is, for its icon (the host picks the picture).
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum ChatFileIcon {
    Document,
    Text,
    Pdf,
    Image,
    Audio,
    Video,
    Archive,
    Code,
    App,
    DiskImage,
    Spreadsheet,
    Presentation,
    Other,
}

/// Where a file stands on this device.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum ChatFileState {
    /// Not here: *Download* or *Save As…*.
    Available,
    Downloading,
    /// Nobody that has it is connected: it goes on when one is.
    Paused,
    /// Here ([`ChatFileCard::path`]).
    Downloaded,
    /// Its sender changed, moved or stopped sharing it.
    Gone,
    /// The download failed ([`ChatFileCard::status`] says why): *Retry*.
    Failed,
    /// Sent from here, and shared from where it is.
    Shared,
    /// Sent from here, and shared no more.
    Unshared,
}

/// What a button of a card does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum ChatFileActionKind {
    /// [`Core::chat_file_download`] into the Downloads folder.
    Download,
    /// Ask where (a save panel named [`Core::chat_file_save_name`]), then [`Core::chat_file_download`] there.
    SaveAs,
    /// [`Core::chat_file_download`] again, where it was going.
    Retry,
    /// [`Core::chat_file_cancel`].
    Cancel,
    /// [`Core::chat_file_open`].
    Open,
    /// Show [`ChatFileCard::path`] in the file manager.
    ShowInFolder,
    /// [`Core::chat_file_stop_sharing`].
    StopSharing,
}

/// One button of a card.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ChatFileAction {
    pub kind: ChatFileActionKind,
    pub label: String,
}

/// One file of a message, ready to draw.
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct ChatFileCard {
    /// Its hash: what every call about it takes.
    pub hash: String,
    /// Its name (another device's, made safe: shown, never used as a path by the host).
    pub name: String,
    /// "1.2 GB".
    pub size_text: String,
    pub icon: ChatFileIcon,
    pub state: ChatFileState,
    /// The line under the name: "Downloading 340 MB of 1.2 GB · 12 MB/s".
    pub status: String,
    /// How far its download is (0 to 1), while it downloads or is paused.
    pub progress: Option<f64>,
    /// Where it is on this device: downloaded to, or shared from.
    pub path: Option<String>,
    /// Its buttons, the first the main one.
    pub actions: Vec<ChatFileAction>,
}

/// A send whose files are being read (or that failed: it then waits to be dismissed).
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct ChatPendingSend {
    pub id: u64,
    /// "Preparing report.zip… 34 %" or "report.zip was not sent".
    pub title: String,
    /// The reason it failed (empty while it is being prepared).
    pub problem: String,
    /// 0 to 1.
    pub progress: f64,
    pub failed: bool,
    /// The dismiss button's label (failed ones).
    pub dismiss: String,
}

// Progress is never NaN (a fraction of a known size, clamped), so equality is total.
impl Eq for ChatFileCard {}
impl Eq for ChatPendingSend {}

/// Opening a file: its path, and what to ask first when it would run code.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ChatFileOpen {
    pub path: String,
    /// Ask this before opening (an app, an installer, a script); `None`: open it.
    pub confirm: Option<ChatFileConfirm>,
}

/// A question before opening a file.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ChatFileConfirm {
    pub title: String,
    pub message: String,
    /// The button that opens it.
    pub open: String,
    pub cancel: String,
}

fn failed(msg: impl Into<String>) -> CoreError {
    CoreError::Failed { message: msg.into() }
}

fn sync_failed(e: seedwell_sync_net::SyncError) -> CoreError {
    failed(seedwell_control::redact::scrub_keys(&e.to_string()))
}

/// A channel's id or `dm:<device>` (a message's `place`).
pub(crate) fn place(s: &str) -> Result<Place, CoreError> {
    s.parse().map_err(|_| failed(concat!("This conversation is no longer on this ", seedwell_platform::computer!(), ".")))
}

/// The icon a file's name calls for.
pub(crate) fn icon_of(name: &str) -> ChatFileIcon {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "pdf" => ChatFileIcon::Pdf,
        "txt" | "md" | "rtf" | "log" | "csv" | "tsv" => ChatFileIcon::Text,
        "doc" | "docx" | "pages" | "odt" => ChatFileIcon::Document,
        "xls" | "xlsx" | "numbers" | "ods" => ChatFileIcon::Spreadsheet,
        "ppt" | "pptx" | "key" | "odp" => ChatFileIcon::Presentation,
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "heic" | "heif" | "tif" | "tiff" | "bmp" | "raw" | "dng" | "psd" => ChatFileIcon::Image,
        "mp3" | "m4a" | "aac" | "wav" | "flac" | "aiff" | "ogg" | "opus" => ChatFileIcon::Audio,
        "mp4" | "mov" | "m4v" | "mkv" | "avi" | "webm" => ChatFileIcon::Video,
        "zip" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "7z" | "rar" | "tar" => ChatFileIcon::Archive,
        "dmg" | "iso" | "img" => ChatFileIcon::DiskImage,
        "app" | "pkg" | "mpkg" | "exe" | "msi" | "msix" | "appimage" | "deb" | "rpm" => ChatFileIcon::App,
        "rs" | "swift" | "c" | "h" | "cpp" | "py" | "js" | "ts" | "go" | "java" | "kt" | "rb" | "sh" | "json" | "toml" | "yaml" | "yml" | "html" | "css" | "xml" => {
            ChatFileIcon::Code
        }
        _ => ChatFileIcon::Other,
    }
}

fn size(n: u64) -> String {
    seedwell_media::size_text(n)
}

fn action(kind: ChatFileActionKind) -> ChatFileAction {
    let label = match kind {
        ChatFileActionKind::Download => "Download",
        ChatFileActionKind::SaveAs => "Save As…",
        ChatFileActionKind::Retry => "Retry",
        ChatFileActionKind::Cancel => "Cancel",
        ChatFileActionKind::Open => "Open",
        ChatFileActionKind::ShowInFolder => concat!("Show in ", seedwell_platform::file_manager!()),
        ChatFileActionKind::StopSharing => "Stop Sharing",
    };
    ChatFileAction { kind, label: label.into() }
}

/// The folder a path is in, as people read it (`~/Downloads`).
fn folder_of(path: &str) -> String {
    Path::new(path).parent().map(seedwell_platform::dirs::shown).unwrap_or_default()
}

/// A file of a message as a card. `author`: who sent it (its device's name), for "waiting for …".
pub(crate) fn card_of(a: &AttachmentInfo, author: &str) -> ChatFileCard {
    use ChatFileActionKind as K;
    let total = size(a.size);
    let progress = (a.size > 0).then(|| (a.done as f64 / a.size as f64).clamp(0.0, 1.0)).or(Some(0.0));
    let (state, status, progress, actions) = match a.state.as_str() {
        "downloading" => {
            let rate = if a.rate > 0 { format!(" · {}/s", size(a.rate)) } else { String::new() };
            (ChatFileState::Downloading, format!("Downloading {} of {total}{rate}", size(a.done)), progress, vec![action(K::Cancel)])
        }
        "paused" => {
            let who = if author.is_empty() { "a device that has it".to_string() } else { author.to_string() };
            (ChatFileState::Paused, format!("Paused at {} of {total}: it goes on when {who} is online", size(a.done)), progress, vec![action(K::Cancel)])
        }
        "downloaded" => {
            let at = a.path.as_deref().map(folder_of).unwrap_or_default();
            (ChatFileState::Downloaded, format!("{total} · Downloaded to {at}"), None, vec![action(K::Open), action(K::ShowInFolder)])
        }
        "gone" => (ChatFileState::Gone, format!("{total} · No longer available: its sender changed, moved or stopped sharing it"), None, Vec::new()),
        "failed" => {
            let why = a.problem.as_deref().unwrap_or("it stopped");
            (ChatFileState::Failed, format!("Download failed: {why}"), None, vec![action(K::Retry), action(K::Cancel)])
        }
        "shared" => {
            let from = a.path.as_deref().map(|p| seedwell_platform::dirs::shown(Path::new(p))).unwrap_or_default();
            (ChatFileState::Shared, format!("{total} · Shared from {from}"), None, vec![action(K::ShowInFolder), action(K::StopSharing)])
        }
        "unshared" => {
            let why = a.problem.as_deref().unwrap_or("it is no longer shared");
            (ChatFileState::Unshared, format!("{total} · No longer shared: {why}"), None, Vec::new())
        }
        _ => (ChatFileState::Available, format!("{total} · Not downloaded"), None, vec![action(K::Download), action(K::SaveAs)]),
    };
    ChatFileCard { hash: a.hash.clone(), name: a.name.clone(), size_text: total, icon: icon_of(&a.name), state, status, progress, path: a.path.clone(), actions }
}

/// A pending send for the host.
pub(crate) fn pending_of(p: &seedwell_sync_net::files::PreparingInfo) -> ChatPendingSend {
    let names = match p.names.as_slice() {
        [one] => one.clone(),
        [first, rest @ ..] => format!("{first} and {} more", rest.len()),
        [] => "the files".into(),
    };
    let progress = if p.total == 0 { 1.0 } else { (p.done as f64 / p.total as f64).clamp(0.0, 1.0) };
    match &p.problem {
        None => ChatPendingSend {
            id: p.id,
            title: format!("Preparing {names}… {} %", (progress * 100.0).floor() as u32),
            problem: String::new(),
            progress,
            failed: false,
            dismiss: "Dismiss".into(),
        },
        Some(why) => ChatPendingSend { id: p.id, title: format!("{names} was not sent"), problem: why.clone(), progress, failed: true, dismiss: "Dismiss".into() },
    }
}

/// What to ask before opening a file that runs code.
pub(crate) fn open_check(path: &Path, name: &str) -> Option<ChatFileConfirm> {
    seedwell_sync_net::files::opens_as_code(path).then(|| ChatFileConfirm {
        title: format!("Open “{name}”?"),
        message: "It is an app, an installer or a script: opening it can run code on this computer. Open it only if you trust the person who sent it.".into(),
        open: "Open".into(),
        cancel: "Cancel".into(),
    })
}

/// What a composer holds, as the core sends it.
fn new_attachment(i: &ChatImageInput) -> Result<NewAttachment, CoreError> {
    match (&i.path, &i.bytes) {
        (Some(p), _) => Ok(NewAttachment::Path(PathBuf::from(p))),
        (None, Some(b)) => Ok(NewAttachment::Image(NewImage { name: i.name.clone(), bytes: b.clone() })),
        (None, None) => Err(failed(format!("{}: nothing to attach", i.name))),
    }
}

/// One fixture card, named for its file in a snapshot.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ChatFileSample {
    pub label: String,
    pub card: ChatFileCard,
}

/// Fixture cards, pending sends and the question before opening a script, made by the same code as the real ones from
/// made-up files: for snapshots and previews only (no chat state is read).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ChatFileSamples {
    pub cards: Vec<ChatFileSample>,
    pub pending: Vec<ChatPendingSend>,
    pub confirm: ChatFileConfirm,
}

/// A made-up file of a message, as the service reports it.
pub(crate) fn sample_info(name: &str, size: u64, state: &str, done: u64, rate: u64, path: Option<&str>, problem: Option<&str>) -> AttachmentInfo {
    AttachmentInfo {
        hash: seedwell_sync_proto::members::hex(&seedwell_sync_net::blobs::sha256(name.as_bytes())),
        kind: "file".into(),
        name: name.into(),
        size,
        width: 0,
        height: 0,
        state: state.into(),
        problem: problem.map(String::from),
        done,
        rate,
        path: path.map(String::from),
        thumb: Vec::new(),
    }
}

/// See [`ChatFileSamples`].
#[uniffi::export]
pub fn chat_file_samples() -> ChatFileSamples {
    let home = seedwell_platform::dirs::home().unwrap_or_default().display().to_string();
    let downloads = format!("{home}/Downloads/Film.mov");
    let shared = format!("{home}/Documents/Budget 2027.xlsx");
    let gb = 1_234_000_000;
    let cards = [
        ("available", sample_info("Film.mov", gb, "available", 0, 0, None, None), "Anna's Mac"),
        ("shared", sample_info("Budget 2027.xlsx", 2_400_000, "shared", 0, 0, Some(&shared), None), ""),
        ("downloading", sample_info("Film.mov", gb, "downloading", 340_000_000, 12_300_000, None, None), "Anna's Mac"),
        ("paused", sample_info("Film.mov", gb, "paused", 340_000_000, 0, None, None), "Anna's Mac"),
        ("downloaded", sample_info("Film.mov", gb, "downloaded", gb, 0, Some(&downloads), None), "Anna's Mac"),
        ("gone", sample_info("draft.key", 48_000_000, "gone", 0, 0, None, None), "Anna's Mac"),
        ("failed", sample_info("backup.zip", 52_000_000_000, "failed", 0, 0, None, Some("there is not enough space: it needs 52 GB, 31 GB is free")), "Anna's Mac"),
        ("unshared", sample_info("notes.md", 4_200, "unshared", 0, 0, Some(&format!("{home}/notes.md")), Some("the file changed since it was sent")), ""),
    ]
    .into_iter()
    .map(|(label, a, author)| ChatFileSample { label: label.into(), card: card_of(&a, author) })
    .collect();
    let preparing = seedwell_sync_net::files::PreparingInfo { id: 1, names: vec!["Film.mov".into()], done: 419_560_000, total: gb, problem: None };
    let failed = seedwell_sync_net::files::PreparingInfo {
        id: 2,
        names: vec!["Pasted Image.png".into()],
        done: 0,
        total: 240_000,
        problem: Some(concat!("that device is not connected to this ", seedwell_platform::computer!(), " directly; private messages go only over a direct connection between the two, never through another device, so write once both are online and can reach each other").into()),
    };
    ChatFileSamples {
        cards,
        pending: vec![pending_of(&preparing), pending_of(&failed)],
        confirm: open_check(Path::new("setup.sh"), "setup.sh").unwrap_or(ChatFileConfirm { title: String::new(), message: String::new(), open: String::new(), cancel: String::new() }),
    }
}

impl Core {
    /// Posts a text message with images and files to `place` (a channel's id, with `topic`; or `dm:<device>`), as the
    /// person at this computer, without waiting for the files to be read: until the message is posted
    /// [`Core::chat_pending_sends`] shows them, and a send that fails stays there with its reason. What can be told at
    /// once (too many, a private conversation whose device is not connected) is an error here.
    pub fn chat_send_attachments(&self, place: String, topic: Option<String>, body: String, reply_to: Option<String>, inputs: Vec<ChatImageInput>) -> Result<(), CoreError> {
        let (h, rt) = self.backend.sync().chat_service().ok_or_else(|| failed("Folder sync is not running."))?;
        let place = self::place(&place)?;
        let max = seedwell_sync_proto::attach::MAX_ATTACHMENTS;
        if inputs.len() > max {
            return Err(failed(format!("A message can have at most {max} attachments.")));
        }
        let items = inputs.iter().map(new_attachment).collect::<Result<Vec<_>, _>>()?;
        if let Place::Direct(d) = place {
            h.chat_direct_ready(&d).map_err(sync_failed)?;
        }
        rt.spawn(async move {
            // A failure of the files shows in the pending sends; one of the message itself is said there too.
            if let Err(e) = h.chat_send_attachments(place, topic.as_deref(), &body, reply_to.as_deref(), None, items).await {
                eprintln!("seedwell chat: a message with attachments was not sent: {e}");
            }
        });
        Ok(())
    }

    /// *Download* (into the Downloads folder), *Retry*, or *Save As…* to `to` (the file the save panel chose, which
    /// replaces one there). Returns the card as it stands now.
    pub fn chat_file_download(&self, place: String, hash: String, to: Option<String>) -> Result<ChatFileCard, CoreError> {
        let (h, _) = self.backend.sync().chat_service().ok_or_else(|| failed("Folder sync is not running."))?;
        let to = to.map(|t| DownloadTo::File(PathBuf::from(t)));
        let a = h.chat_file_download(self::place(&place)?, &hash, to).map_err(sync_failed)?;
        Ok(card_of(&a, ""))
    }

    /// *Cancel*: the download stops and its partial file goes.
    pub fn chat_file_cancel(&self, place: String, hash: String) -> Result<ChatFileCard, CoreError> {
        let (h, _) = self.backend.sync().chat_service().ok_or_else(|| failed("Folder sync is not running."))?;
        Ok(card_of(&h.chat_file_cancel(self::place(&place)?, &hash).map_err(sync_failed)?, ""))
    }

    /// *Stop Sharing* a file sent from here: those who ask for it are told it is gone.
    pub fn chat_file_stop_sharing(&self, place: String, hash: String) -> Result<ChatFileCard, CoreError> {
        let (h, _) = self.backend.sync().chat_service().ok_or_else(|| failed("Folder sync is not running."))?;
        Ok(card_of(&h.chat_file_stop_sharing(self::place(&place)?, &hash).map_err(sync_failed)?, ""))
    }

    /// *Open*: the file's path, and what to ask first when it is an app, an installer or a script. Never opened here.
    pub fn chat_file_open(&self, place: String, hash: String) -> Result<ChatFileOpen, CoreError> {
        let (h, _) = self.backend.sync().chat_service().ok_or_else(|| failed("Folder sync is not running."))?;
        let (a, _) = h.chat_attachment(self::place(&place)?, &hash).map_err(sync_failed)?;
        let path = a.path.clone().filter(|p| Path::new(p).exists()).ok_or_else(|| failed(concat!("The file is not on this ", seedwell_platform::computer!(), ".")))?;
        Ok(ChatFileOpen { confirm: open_check(Path::new(&path), &a.name), path })
    }
}

#[uniffi::export]
impl Core {
    /// The sends of a channel or conversation whose files are being read, or that failed (until dismissed).
    pub fn chat_pending_sends(&self, place: String) -> Vec<ChatPendingSend> {
        let (Some((h, _)), Ok(p)) = (self.backend.sync().chat_service(), self::place(&place)) else { return Vec::new() };
        h.chat_preparing(p).iter().map(pending_of).collect()
    }

    /// Forgets a send that failed.
    pub fn chat_dismiss_pending(&self, place: String, id: u64) {
        if let (Some((h, _)), Ok(p)) = (self.backend.sync().chat_service(), self::place(&place)) {
            h.chat_dismiss_preparing(p, id);
        }
    }

    /// The name *Save As…* suggests.
    pub fn chat_file_save_name(&self, place: String, hash: String) -> String {
        self.backend
            .sync()
            .chat_service()
            .and_then(|(h, _)| h.chat_attachment(self::place(&place).ok()?, &hash).ok())
            .map_or_else(|| "file".into(), |(a, _)| a.name)
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(state: &str, done: u64, rate: u64, path: Option<&str>, problem: Option<&str>) -> AttachmentInfo {
        AttachmentInfo {
            hash: "cd".repeat(32),
            kind: "file".into(),
            name: "Film.mov".into(),
            size: 1_200_000_000,
            width: 0,
            height: 0,
            state: state.into(),
            problem: problem.map(String::from),
            done,
            rate,
            path: path.map(String::from),
            thumb: Vec::new(),
        }
    }

    fn labels(c: &ChatFileCard) -> Vec<&str> {
        c.actions.iter().map(|a| a.label.as_str()).collect()
    }

    #[test]
    fn every_state_of_a_file_has_its_words_and_buttons() {
        let c = card_of(&info("available", 0, 0, None, None), "Anna's Mac");
        assert_eq!((c.state, c.icon, c.size_text.as_str(), c.status.as_str(), c.progress), (ChatFileState::Available, ChatFileIcon::Video, "1.2 GB", "1.2 GB · Not downloaded", None));
        assert_eq!(labels(&c), ["Download", "Save As…"]);

        let c = card_of(&info("downloading", 340_000_000, 12_000_000, None, None), "Anna's Mac");
        assert_eq!((c.state, c.status.as_str(), labels(&c)), (ChatFileState::Downloading, "Downloading 340 MB of 1.2 GB · 12 MB/s", vec!["Cancel"]));
        assert!((c.progress.unwrap() - 0.2833).abs() < 0.001);

        let c = card_of(&info("paused", 340_000_000, 0, None, None), "Anna's Mac");
        assert_eq!((c.state, c.status.as_str()), (ChatFileState::Paused, "Paused at 340 MB of 1.2 GB: it goes on when Anna's Mac is online"));

        let c = card_of(&info("downloaded", 1_200_000_000, 0, Some("/Users/bo/Downloads/Film.mov"), None), "Anna's Mac");
        assert_eq!((c.state, c.status.as_str(), c.path.as_deref()), (ChatFileState::Downloaded, "1.2 GB · Downloaded to /Users/bo/Downloads", Some("/Users/bo/Downloads/Film.mov")));
        assert_eq!(labels(&c), ["Open", concat!("Show in ", seedwell_platform::file_manager!())]);

        let c = card_of(&info("gone", 0, 0, None, None), "");
        assert_eq!((c.state, c.status.as_str(), c.actions.len()), (ChatFileState::Gone, "1.2 GB · No longer available: its sender changed, moved or stopped sharing it", 0));

        let c = card_of(&info("failed", 0, 0, None, Some("there is not enough space: it needs 1.2 GB, 300 MB is free")), "");
        assert_eq!((c.state, c.status.as_str(), labels(&c)), (ChatFileState::Failed, "Download failed: there is not enough space: it needs 1.2 GB, 300 MB is free", vec!["Retry", "Cancel"]));

        let c = card_of(&info("shared", 0, 0, Some("/Volumes/Work/Film.mov"), None), "");
        assert_eq!((c.state, c.status.as_str()), (ChatFileState::Shared, "1.2 GB · Shared from /Volumes/Work/Film.mov"));
        assert_eq!(labels(&c), [concat!("Show in ", seedwell_platform::file_manager!()), "Stop Sharing"]);

        let c = card_of(&info("unshared", 0, 0, Some("/x/Film.mov"), Some("the file changed since it was sent")), "");
        assert_eq!((c.state, c.status.as_str()), (ChatFileState::Unshared, "1.2 GB · No longer shared: the file changed since it was sent"));
        let c = card_of(&info("paused", 0, 0, None, None), "");
        assert!(c.status.ends_with("when a device that has it is online"));
    }

    #[test]
    fn icons_follow_the_name_and_pending_sends_say_how_far() {
        for (n, i) in [("a.PDF", ChatFileIcon::Pdf), ("b.zip", ChatFileIcon::Archive), ("c.dmg", ChatFileIcon::DiskImage), ("d.app", ChatFileIcon::App), ("e", ChatFileIcon::Other), ("f.rs", ChatFileIcon::Code)] {
            assert_eq!(icon_of(n), i, "{n}");
        }
        let p = seedwell_sync_net::files::PreparingInfo { id: 3, names: vec!["big.iso".into(), "notes.txt".into()], done: 340, total: 1000, problem: None };
        let v = pending_of(&p);
        assert_eq!((v.title.as_str(), v.failed, v.progress), ("Preparing big.iso and 1 more… 34 %", false, 0.34));
        let bad = pending_of(&seedwell_sync_net::files::PreparingInfo { problem: Some("big.iso: the file changed while it was being read; try again".into()), names: vec!["big.iso".into()], ..p });
        assert_eq!((bad.title.as_str(), bad.failed, bad.problem.as_str(), bad.dismiss.as_str()), ("big.iso was not sent", true, "big.iso: the file changed while it was being read; try again", "Dismiss"));
    }

    #[test]
    fn the_samples_cover_every_state_and_say_what_the_real_ones_say() {
        let s = chat_file_samples();
        let states: Vec<ChatFileState> = s.cards.iter().map(|c| c.card.state).collect();
        use ChatFileState::*;
        assert_eq!(states, [Available, Shared, Downloading, Paused, Downloaded, Gone, Failed, Unshared]);
        assert_eq!(s.pending[0].title, "Preparing Film.mov… 34 %");
        assert!(s.pending[1].failed && s.pending[1].problem.contains("directly"));
        assert_eq!(s.confirm.title, "Open “setup.sh”?");
    }

    #[test]
    fn opening_an_app_or_a_script_asks_first() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("setup.sh");
        let doc = dir.path().join("report.pdf");
        std::fs::write(&script, b"rm -rf ~").unwrap();
        std::fs::write(&doc, b"%PDF").unwrap();
        let ask = open_check(&script, "setup.sh").unwrap();
        assert_eq!((ask.title.as_str(), ask.open.as_str(), ask.cancel.as_str()), ("Open “setup.sh”?", "Open", "Cancel"));
        assert!(ask.message.contains("can run code"));
        assert_eq!(open_check(&doc, "report.pdf"), None);
    }
}
