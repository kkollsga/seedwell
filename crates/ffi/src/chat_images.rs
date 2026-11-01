//! Images in chat for the hosts (SPEC §7G.7 *Attachments*): what a message's images are and say, the pictures to
//! draw, and attaching, saving and opening them. Rust decides (checks, caps, texts, states); the host draws only the
//! PNG or JPEG bytes made or checked here, never an SVG or another device's bytes as they came.
//!
//! * A message's images are [`ChatImage`]s in [`SyncMessage::images`], with their state and the texts to show.
//! * Composer: [`Core::chat_image_check`] for a pasted, dropped or picked image or file (its chip),
//!   [`Core::chat_send_attachments`] to post ([`crate::chat_files`]).
//! * Every call takes the message's `place` as `channel_id`: a channel's id, or `dm:<device>` for a private message.
//! * Drawing: [`Core::chat_image_preview`] (the message's small preview, for the placeholder while the image is on its
//!   way) and [`Core::chat_image_view`] (the picture at a size).
//! * Actions: [`Core::chat_image_fetch`] (Download), [`Core::chat_image_save`] (Save As…), [`Core::chat_image_open_path`]
//!   (a file for Quick Look or the default viewer), [`Core::chat_image_texts`] (every label).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};

use trs_sync_net::SyncHandle;
use trs_sync_net::chat::AttachmentInfo;
use trs_sync_net::room::Place;

use crate::Core;
use crate::chat_files::ChatFileIcon;
use crate::types::CoreError;

/// An image's format.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum ChatImageKind {
    Png,
    Jpeg,
    Gif,
    Webp,
    /// Drawn here as PNG ([`Core::chat_image_view`]); saved as the SVG it is, or as PNG.
    Svg,
    /// A kind of a later version: shown as its placeholder only.
    Other,
}

/// Where an image stands on this device.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum ChatImageState {
    /// Here: draw [`Core::chat_image_view`].
    Available,
    /// On its way, or waiting for a device that has it: draw the preview with [`ChatImage::status`].
    Fetching,
    /// Older than what the channel keeps by itself: offer *Download* ([`Core::chat_image_fetch`]).
    NotDownloaded,
    /// Larger than an image may be: never fetched.
    TooLarge,
    /// It cannot be shown ([`ChatImage::status`] says why); *Download* tries again.
    Failed,
}

/// One image of a message.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ChatImage {
    /// Its hash: what every call about it takes.
    pub hash: String,
    pub kind: ChatImageKind,
    /// Its file name (another device's: shown, never used as a path).
    pub name: String,
    /// Bytes.
    pub size: u64,
    /// Pixels: the picture's aspect ratio, for its place before it is drawn.
    pub width: u32,
    pub height: u32,
    pub state: ChatImageState,
    /// One line about it, ready to show: "logo.svg · 40 × 20 · 12 KB".
    pub caption: String,
    /// What its placeholder says while it is not drawn ("Downloading…"); empty when it is here.
    pub status: String,
    /// The message carries a small preview of it ([`Core::chat_image_preview`]).
    pub has_preview: bool,
}

/// An image to attach: from a file (`path`) or from the pasteboard (`bytes`), with the name it goes under.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ChatImageInput {
    /// Its file name (a pasted screenshot: any name; the extension follows its format).
    pub name: String,
    pub path: Option<String>,
    pub bytes: Option<Vec<u8>>,
}

/// An image or file checked for the composer: its chip.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ChatImageDraft {
    /// The name it goes under.
    pub name: String,
    /// It goes as a file (shared from where it is), not as an image: draw [`ChatImageDraft::icon`].
    pub file: bool,
    pub icon: ChatFileIcon,
    pub kind: ChatImageKind,
    pub width: u32,
    pub height: u32,
    /// Bytes, as sent (an image's metadata stripped).
    pub size: u64,
    /// "Screenshot.png · 1280 × 800 · 240 KB", "Film.mov · 1.2 GB".
    pub caption: String,
    /// A small PNG of it for the chip (empty: none).
    pub preview_png: Vec<u8>,
}

/// A picture to draw: PNG or JPEG bytes made or checked here.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ChatImageView {
    pub bytes: Vec<u8>,
    /// `image/png` or `image/jpeg`.
    pub mime: String,
    pub width: u32,
    pub height: u32,
}

/// Every label of images in chat.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ChatImageTexts {
    /// The composer's button and menu item.
    pub attach: String,
    /// Its tooltip.
    pub attach_help: String,
    /// Shown over the composer while images are dragged onto it.
    pub drop_hint: String,
    /// A chip's remove button (accessibility label).
    pub remove: String,
    pub save_as: String,
    /// For an SVG: the picture as PNG.
    pub save_as_png: String,
    pub copy_image: String,
    pub open: String,
    pub download: String,
    /// The most attachments one message may have, and what is said when more are added.
    pub max_images: u32,
    pub too_many: String,
    /// The file types the attach panel offers (extensions); empty: any file.
    pub extensions: Vec<String>,
    /// The name a pasted picture goes under (its extension follows its format).
    pub pasted_name: String,
}

fn failed(msg: impl Into<String>) -> CoreError {
    CoreError::Failed { message: msg.into() }
}

fn kind_of(k: Option<trs_media::Kind>) -> ChatImageKind {
    match k {
        Some(trs_media::Kind::Png) => ChatImageKind::Png,
        Some(trs_media::Kind::Jpeg) => ChatImageKind::Jpeg,
        Some(trs_media::Kind::Gif) => ChatImageKind::Gif,
        Some(trs_media::Kind::Webp) => ChatImageKind::Webp,
        Some(trs_media::Kind::Svg) => ChatImageKind::Svg,
        None => ChatImageKind::Other,
    }
}

/// "name · W × H · size".
fn caption(name: &str, width: u32, height: u32, size: u64) -> String {
    format!("{name} · {width} × {height} · {}", trs_media::size_text(size))
}

/// An image of a message for the host. `waiting`: no other member of the channel is connected now.
pub(crate) fn image_of(a: &AttachmentInfo, waiting: bool) -> ChatImage {
    let state = match a.state.as_str() {
        "available" => ChatImageState::Available,
        "fetching" => ChatImageState::Fetching,
        "not_downloaded" => ChatImageState::NotDownloaded,
        "too_large" => ChatImageState::TooLarge,
        _ => ChatImageState::Failed,
    };
    let kind = a.media_kind();
    let status = match state {
        ChatImageState::Available => String::new(),
        ChatImageState::Fetching if waiting => "Waiting for a device that has this image to come online".into(),
        ChatImageState::Fetching => "Downloading…".into(),
        ChatImageState::NotDownloaded => "Not downloaded".into(),
        ChatImageState::TooLarge => format!("Too large to show ({})", trs_media::size_text(a.size)),
        ChatImageState::Failed if kind.is_none() => "This image needs a newer Seedwell".into(),
        ChatImageState::Failed => format!("Cannot be shown: {}", a.problem.as_deref().unwrap_or("it is damaged")),
    };
    ChatImage {
        hash: a.hash.clone(),
        kind: kind_of(kind),
        name: a.name.clone(),
        size: a.size,
        width: a.width,
        height: a.height,
        state,
        caption: caption(&a.name, a.width, a.height, a.size),
        status,
        has_preview: !a.thumb.is_empty(),
    }
}

/// The labels (the same on every host).
pub(crate) fn texts() -> ChatImageTexts {
    let max = trs_sync_proto::attach::MAX_ATTACHMENTS as u32;
    ChatImageTexts {
        attach: "Attach Files…".into(),
        attach_help: "Attach files of any kind and size, also by pasting or dragging them here. Images are shown in the conversation; other files are shared from where they are, and others download them when they choose.".into(),
        drop_hint: "Drop files to attach them".into(),
        remove: "Remove".into(),
        save_as: "Save As…".into(),
        save_as_png: "Save as PNG…".into(),
        copy_image: "Copy Image".into(),
        open: "Open".into(),
        download: "Download".into(),
        max_images: max,
        too_many: format!("A message can have at most {max} attachments."),
        extensions: Vec::new(),
        pasted_name: "Pasted Image.png".into(),
    }
}

/// The name an image is called in a sentence: the last part of the one given.
fn shown(name: &str) -> String {
    name.rsplit(['/', '\\']).next().filter(|n| !n.trim().is_empty()).unwrap_or("image").to_string()
}

/// An input's bytes when it may be an image (as given, or read from a file no larger than an image may be); `None`:
/// a file that goes as a file. A folder is refused.
fn bytes_of(i: &ChatImageInput) -> Result<Option<Vec<u8>>, CoreError> {
    let shown = shown(&i.name);
    match (&i.bytes, &i.path) {
        (Some(b), _) => Ok(Some(b.clone())),
        (None, Some(p)) => {
            let m = std::fs::metadata(p).map_err(|e| failed(format!("{shown}: {e}")))?;
            if m.is_dir() {
                return Err(failed(trs_sync_net::attachments::folder_refused(&shown)));
            }
            if m.len() > trs_media::MAX_IMAGE as u64 {
                return Ok(None);
            }
            std::fs::read(p).map(Some).map_err(|e| failed(format!("{shown}: {e}")))
        }
        (None, None) => Err(failed(format!("{shown}: nothing to attach"))),
    }
}

fn view_of(v: trs_media::View) -> ChatImageView {
    ChatImageView { bytes: v.bytes, mime: v.mime.into(), width: v.width, height: v.height }
}

/// The last pictures made for the host, by (channel, hash, size): drawing a large picture again on every refresh would
/// be slow. Bounded by [`CACHE_BYTES`].
const CACHE_BYTES: usize = 64 << 20;
type Cache = Mutex<VecDeque<((String, String, u32), Arc<ChatImageView>)>>;

fn cache() -> &'static Cache {
    static CACHE: OnceLock<Cache> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(VecDeque::new()))
}

fn cached(key: &(String, String, u32)) -> Option<Arc<ChatImageView>> {
    let c = cache().lock().unwrap_or_else(|e| e.into_inner());
    c.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
}

fn remember(key: (String, String, u32), v: Arc<ChatImageView>) {
    let mut c = cache().lock().unwrap_or_else(|e| e.into_inner());
    c.retain(|(k, _)| *k != key);
    c.push_back((key, v));
    while c.iter().map(|(_, v)| v.bytes.len()).sum::<usize>() > CACHE_BYTES && c.len() > 1 {
        c.pop_front();
    }
}

impl Core {
    fn chat_service(&self) -> Result<(SyncHandle, tokio::runtime::Handle), CoreError> {
        self.backend.sync().chat_service().ok_or_else(|| failed("Folder sync is not running."))
    }
}

fn channel(id: &str) -> Result<Place, CoreError> {
    crate::chat_files::place(id)
}

fn sync_failed(e: trs_sync_net::SyncError) -> CoreError {
    failed(trs_control::redact::scrub_keys(&e.to_string()))
}

#[uniffi::export]
impl Core {
    /// Every label of images in chat.
    pub fn chat_image_texts(&self) -> ChatImageTexts {
        texts()
    }

    /// Checks what is attached in the composer (pasted, dropped or picked): its chip, or why it cannot be sent. An
    /// image within the caps is an image (its format found from its bytes; it must decode); any other file, or an
    /// image over the caps, goes as a file of any size; a folder and pasted bytes that are no image are refused. The
    /// same checks run again when it is sent.
    pub fn chat_image_check(&self, image: ChatImageInput) -> Result<ChatImageDraft, CoreError> {
        let as_file = |path: &str| -> Result<ChatImageDraft, CoreError> {
            let size = std::fs::metadata(path).map_err(|e| failed(format!("{}: {e}", shown(&image.name))))?.len();
            let name = trs_sync_proto::attach::safe_name(&shown(&image.name));
            Ok(ChatImageDraft {
                caption: format!("{name} · {}", trs_media::size_text(size)),
                icon: crate::chat_files::icon_of(&name),
                name,
                file: true,
                kind: ChatImageKind::Other,
                width: 0,
                height: 0,
                size,
                preview_png: Vec::new(),
            })
        };
        let Some(bytes) = bytes_of(&image)? else { return as_file(image.path.as_deref().unwrap_or_default()) };
        let p = match (trs_media::prepare(&image.name, &bytes), image.path.as_deref()) {
            (Ok(p), _) => p,
            // A file that is no image (or none this version sends as one) goes as a file.
            (Err(_), Some(path)) => return as_file(path),
            (Err(e), None) => return Err(failed(format!("{}: {e}", shown(&image.name)))),
        };
        let preview_png = trs_media::thumb_png(&p.thumb).map(|v| v.bytes).unwrap_or_default();
        Ok(ChatImageDraft {
            caption: caption(&p.name, p.width, p.height, p.bytes.len() as u64),
            icon: ChatFileIcon::Image,
            file: false,
            name: p.name,
            kind: kind_of(Some(p.kind)),
            width: p.width,
            height: p.height,
            size: p.bytes.len() as u64,
            preview_png,
        })
    }

    /// The small preview a message carries for an image, checked and made a PNG: what its placeholder shows while the
    /// image is on its way. `None`: it has none.
    pub fn chat_image_preview(&self, channel_id: String, hash: String) -> Option<ChatImageView> {
        let (h, _) = self.chat_service().ok()?;
        let jpeg = h.chat_attachment_preview(channel(&channel_id).ok()?, &hash).ok()?;
        (!jpeg.is_empty()).then(|| trs_media::thumb_png(&jpeg).ok().map(view_of)).flatten()
    }

    /// The picture of an image that is here, at most `max_side` pixels on its longest side (0: as large as it is, up
    /// to 4096): PNG or JPEG, an SVG drawn here, a GIF or WebP's first frame. `None` while it is not here (its
    /// [`ChatImage::state`] says why). Kept for the next call; drawing a large one may take a moment, so call it off the
    /// main thread.
    pub fn chat_image_view(&self, channel_id: String, hash: String, max_side: u32) -> Option<ChatImageView> {
        let key = (channel_id.clone(), hash.clone(), max_side);
        if let Some(v) = cached(&key) {
            return Some((*v).clone());
        }
        let (h, _) = self.chat_service().ok()?;
        let (info, bytes) = h.chat_attachment(channel(&channel_id).ok()?, &hash).ok()?;
        let view = Arc::new(view_of(trs_media::render(info.media_kind()?, &bytes?, max_side).ok()?));
        remember(key, view.clone());
        Some((*view).clone())
    }

    /// Fetches an image now (*Download*: one older than what the channel keeps by itself, or one that failed).
    pub fn chat_image_fetch(&self, channel_id: String, hash: String) -> Result<(), CoreError> {
        let (h, _) = self.chat_service()?;
        h.chat_attachment_fetch(channel(&channel_id)?, &hash).map_err(sync_failed)?;
        Ok(())
    }

    /// The name *Save As…* suggests: the image's, or for `as_png` its name with `.png`.
    pub fn chat_image_save_name(&self, channel_id: String, hash: String, as_png: bool) -> String {
        let name = self
            .chat_service()
            .ok()
            .and_then(|(h, _)| h.chat_attachment(channel(&channel_id).ok()?, &hash).ok())
            .map_or_else(|| "image".to_string(), |(i, _)| i.name);
        if as_png { format!("{}.png", name.rsplit_once('.').map_or(name.as_str(), |(s, _)| s)) } else { name }
    }

    /// *Save As…*: writes the image as it was sent to `path` (metadata already stripped), or for `as_png` an SVG drawn
    /// as PNG.
    pub fn chat_image_save(&self, channel_id: String, hash: String, path: String, as_png: bool) -> Result<(), CoreError> {
        let (h, _) = self.chat_service()?;
        let (info, bytes) = h.chat_attachment(channel(&channel_id)?, &hash).map_err(sync_failed)?;
        let bytes = bytes.ok_or_else(|| failed("The image is not on this computer yet."))?;
        let out = if as_png && info.media_kind() == Some(trs_media::Kind::Svg) {
            trs_media::render(trs_media::Kind::Svg, &bytes, 0).map_err(|e| failed(e.to_string()))?.bytes
        } else {
            bytes.to_vec()
        };
        std::fs::write(&path, out).map_err(|e| failed(format!("The image could not be saved: {e}")))
    }

    /// A file of the picture (PNG or JPEG, as large as it is up to 4096 pixels a side), for Quick Look or the default
    /// viewer: never the SVG or another device's bytes as they came. In the temporary folder, by its hash.
    pub fn chat_image_open_path(&self, channel_id: String, hash: String) -> Result<String, CoreError> {
        let v = self.chat_image_view(channel_id.clone(), hash.clone(), 0).ok_or_else(|| failed("The image is not on this computer yet."))?;
        let name = self.chat_image_save_name(channel_id, hash.clone(), false);
        let stem = name.rsplit_once('.').map_or(name.as_str(), |(s, _)| s).replace(['/', '\\', ':'], "-");
        let ext = if v.mime == "image/jpeg" { "jpg" } else { "png" };
        let dir = std::env::temp_dir().join("Seedwell Images").join(hash.get(..16).unwrap_or(&hash));
        std::fs::create_dir_all(&dir).map_err(|e| failed(e.to_string()))?;
        let path = dir.join(format!("{stem}.{ext}"));
        std::fs::write(&path, &v.bytes).map_err(|e| failed(e.to_string()))?;
        Ok(path.display().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(state: &str, kind: &str) -> AttachmentInfo {
        AttachmentInfo {
            hash: "ab".repeat(32),
            kind: kind.into(),
            name: "logo.svg".into(),
            size: 12_300,
            width: 40,
            height: 20,
            state: state.into(),
            problem: (state == "failed").then(|| "it is not a SVG image".into()),
            done: 0,
            rate: 0,
            path: None,
            thumb: vec![1],
        }
    }

    #[test]
    fn every_state_has_its_text() {
        let i = image_of(&info("available", "svg"), false);
        assert_eq!((i.kind, i.state, i.caption.as_str(), i.status.as_str(), i.has_preview), (ChatImageKind::Svg, ChatImageState::Available, "logo.svg · 40 × 20 · 12 KB", "", true));
        assert_eq!(image_of(&info("fetching", "png"), false).status, "Downloading…");
        assert_eq!(image_of(&info("fetching", "png"), true).status, "Waiting for a device that has this image to come online");
        assert_eq!(image_of(&info("not_downloaded", "png"), false).state, ChatImageState::NotDownloaded);
        assert_eq!(image_of(&info("too_large", "png"), false).status, "Too large to show (12 KB)");
        assert_eq!(image_of(&info("failed", "png"), false).status, "Cannot be shown: it is not a SVG image");
        let later = image_of(&info("failed", "other"), false);
        assert_eq!((later.kind, later.status.as_str()), (ChatImageKind::Other, "This image needs a newer Seedwell"));
        let t = texts();
        assert_eq!((t.max_images, t.too_many.as_str(), t.extensions.len()), (10, "A message can have at most 10 attachments.", 0));
    }

    #[test]
    fn a_file_larger_than_any_image_is_left_unread_and_a_folder_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("big.png");
        std::fs::File::create(&p).unwrap().set_len(trs_media::MAX_IMAGE as u64 + 1).unwrap();
        assert_eq!(bytes_of(&ChatImageInput { name: "big.png".into(), path: Some(p.display().to_string()), bytes: None }).unwrap(), None, "it goes as a file");
        let e = bytes_of(&ChatImageInput { name: "Photos".into(), path: Some(dir.path().display().to_string()), bytes: None }).unwrap_err();
        assert_eq!(e.to_string(), "Photos is a folder. Only files can be attached; to share a folder, use Folder Sync.");
    }

    /// A real core in temporary folders (HOME too), as `lib.rs`'s tests make one.
    fn real_core() -> (Arc<Core>, [tempfile::TempDir; 2]) {
        let home = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        // SAFETY: set before the core starts any thread; nextest runs every test in its own process.
        unsafe { std::env::set_var("HOME", home.path()) };
        let dir = work.path().join("state");
        let store = crate::settings::SettingsStore::new(&dir);
        let mut st = store.load_settings();
        st.folders.download_dir = work.path().join("done").display().to_string();
        st.folders.incomplete_dir = work.path().join("incomplete").display().to_string();
        st.folders.watch.clear();
        st.network.port_mapping = false;
        store.save_settings(&st).unwrap();
        let options = crate::CoreOptions { config_dir: dir.display().to_string(), mock: false, allow_loopback_trackers: false, forbidden_root: None };
        (Core::new(options).unwrap(), [home, work])
    }

    #[test]
    fn the_host_attaches_draws_saves_and_opens_images_made_safe_here() {
        let (core, tmp) = real_core();
        let started = std::time::Instant::now();
        let ch = loop {
            match core.chat_new("Design".into(), false) {
                Ok(c) => break c,
                Err(_) if started.elapsed() < std::time::Duration::from_secs(20) => std::thread::sleep(std::time::Duration::from_millis(100)),
                Err(e) => panic!("{e}"),
            }
        };
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="20"><rect width="40" height="20" fill="#f00"/></svg>"##.to_vec();
        let draft = core.chat_image_check(ChatImageInput { name: "logo".into(), path: None, bytes: Some(svg.clone()) }).unwrap();
        assert_eq!((draft.name.as_str(), draft.kind, draft.caption.clone()), ("logo.svg", ChatImageKind::Svg, format!("logo.svg · 40 × 20 · {} bytes", svg.len())));
        assert!(draft.preview_png.starts_with(b"\x89PNG"));
        let bad = core.chat_image_check(ChatImageInput { name: "/x/notes.txt".into(), path: None, bytes: Some(b"hello".to_vec()) }).unwrap_err();
        assert_eq!(bad.to_string(), "notes.txt: only PNG, JPEG, GIF, WebP and SVG images can be sent");

        // Any other file goes as a file: its chip names it and its size.
        let notes = tmp[1].path().join("notes.txt");
        std::fs::write(&notes, b"hello there").unwrap();
        let d = core.chat_image_check(ChatImageInput { name: "notes.txt".into(), path: Some(notes.display().to_string()), bytes: None }).unwrap();
        assert_eq!((d.file, d.icon, d.caption.as_str(), d.size), (true, ChatFileIcon::Text, "notes.txt · 11 bytes", 11));

        let file = tmp[1].path().join("logo.svg");
        std::fs::write(&file, &svg).unwrap();
        let inputs = vec![
            ChatImageInput { name: "logo.svg".into(), path: Some(file.display().to_string()), bytes: None },
            ChatImageInput { name: "notes.txt".into(), path: Some(notes.display().to_string()), bytes: None },
        ];
        core.chat_send_attachments(ch.id.clone(), None, "".into(), None, inputs).unwrap();
        let started = std::time::Instant::now();
        // Shown as being prepared until it is posted; then the message is there and the line is gone.
        let m = loop {
            if let Some(m) = core.chat_messages(ch.id.clone(), None, None, 10).into_iter().next()
                && core.chat_pending_sends(ch.id.clone()).is_empty()
            {
                break m;
            }
            assert!(started.elapsed() < std::time::Duration::from_secs(20), "not posted");
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        let img = m.images[0].clone();
        let card = m.files[0].clone();
        assert_eq!((m.place.as_str(), card.name.as_str(), card.state), (ch.id.as_str(), "notes.txt", crate::chat_files::ChatFileState::Shared));
        assert_eq!(card.path.as_deref(), Some(notes.display().to_string().as_str()), "shared from where it is");
        let open = core.chat_file_open(ch.id.clone(), card.hash.clone()).unwrap();
        assert_eq!((open.path, open.confirm), (notes.display().to_string(), None));
        let stopped = core.chat_file_stop_sharing(ch.id.clone(), card.hash.clone()).unwrap();
        assert_eq!(stopped.state, crate::chat_files::ChatFileState::Unshared);
        assert_eq!((m.body.as_str(), img.state, img.width, img.height, img.has_preview), ("", ChatImageState::Available, 40, 20, true));

        // Drawn here: the host gets a PNG, never the SVG.
        let v = core.chat_image_view(ch.id.clone(), img.hash.clone(), 200).unwrap();
        assert_eq!((v.mime.as_str(), v.width, v.height), ("image/png", 200, 100));
        assert!(core.chat_image_preview(ch.id.clone(), img.hash.clone()).unwrap().bytes.starts_with(b"\x89PNG"));
        let open = core.chat_image_open_path(ch.id.clone(), img.hash.clone()).unwrap();
        assert!(open.ends_with("logo.png") && std::fs::read(&open).unwrap().starts_with(b"\x89PNG"), "{open}");
        // Saved as it was sent, or drawn as PNG.
        let (svg_out, png_out) = (tmp[1].path().join("out.svg"), tmp[1].path().join("out.png"));
        core.chat_image_save(ch.id.clone(), img.hash.clone(), svg_out.display().to_string(), false).unwrap();
        core.chat_image_save(ch.id.clone(), img.hash.clone(), png_out.display().to_string(), true).unwrap();
        assert_eq!(std::fs::read(&svg_out).unwrap(), svg);
        assert!(std::fs::read(&png_out).unwrap().starts_with(b"\x89PNG"));
        assert_eq!(core.chat_image_save_name(ch.id.clone(), img.hash.clone(), true), "logo.png");
        assert!(core.shutdown(5).finished);
    }
}
