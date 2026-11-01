//! Attachments of chat messages for callers (SPEC §7G.7 *Attachments*): sending images and files, what each is and
//! where it stands, an image's bytes, and downloading, cancelling and sharing files. The same calls serve channels
//! and private conversations ([`Place`]).

use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use trs_sync_proto::attach::{AttachmentRef, Attachments, KIND_FILE, MAX_ATTACHMENTS, MAX_EXTRA, safe_name};
use trs_sync_proto::members::hex;

use crate::blobs::BlobState;
use crate::chat::{AttachmentBytes, AttachmentInfo, MessageInfo, MessageKind, NewAttachment, NewImage};
use crate::files::{DownloadTo, Prepared, PreparingInfo};
use crate::room::{Index, Place, Room};
use crate::service::{SyncError, SyncHandle, invalid};

/// What is said when a folder is attached.
pub fn folder_refused(name: &str) -> String {
    format!("{name} is a folder. Only files can be attached; to share a folder, use Folder Sync.")
}

/// The line a version without attachments shows for one (`[image: shot.png, 240 KB]`, `[file: report.pdf, 1.2 GB]`).
fn fallback_line(file: bool, name: &str, size: u64) -> String {
    format!("[{}: {name}, {}]", if file { "file" } else { "image" }, trs_media::size_text(size))
}

/// An attachment of a message as callers see it; `mine`: the message was sent from this device.
pub(crate) fn attachment_info(room: &dyn Room, ix: &Index, r: &AttachmentRef, mine: bool) -> AttachmentInfo {
    if r.kind == KIND_FILE {
        let v = room.files().view(&r.hash, mine);
        return AttachmentInfo {
            hash: hex(&r.hash),
            kind: "file".into(),
            name: safe_name(&r.name),
            size: r.size,
            width: 0,
            height: 0,
            state: v.state.into(),
            problem: v.problem,
            done: v.done,
            rate: v.rate,
            path: v.path.map(|p| p.display().to_string()),
            thumb: Vec::new(),
        };
    }
    let kind = trs_media::Kind::from_byte(r.kind);
    let state = crate::blobs::state_of(room.blobs(), r.kind, r.size, &r.hash, ix.budget_set.contains(&r.hash));
    AttachmentInfo {
        hash: hex(&r.hash),
        kind: kind.map_or("other", |k| k.name()).into(),
        // Another device's name for it: cleaned like one sent from here.
        name: trs_media::file_name(&r.name, kind.unwrap_or(trs_media::Kind::Png)),
        size: r.size,
        width: r.width,
        height: r.height,
        state: state.name().into(),
        problem: match state {
            BlobState::Failed(why) => Some(why),
            _ => None,
        },
        done: 0,
        rate: 0,
        path: None,
        thumb: r.thumb.clone(),
    }
}

/// An attachment of a room by its hash, or a prefix of it (8 or more hex characters) only it has.
fn find(ix: &Index, s: &str) -> Result<[u8; 32], SyncError> {
    let s = s.trim().to_lowercase();
    if let Some(h) = crate::blobs::hash_from_hex(&s).filter(|h| ix.refs.contains_key(h)) {
        return Ok(h);
    }
    if s.len() < 8 || !s.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(invalid("no such attachment (give its hash, or at least its first 8 characters)"));
    }
    let hits: Vec<[u8; 32]> = ix.refs.keys().filter(|h| hex(&h[..]).starts_with(&s)).copied().take(2).collect();
    match hits.as_slice() {
        [h] => Ok(*h),
        [] => Err(invalid("no such attachment here (its message may be too old)")),
        _ => Err(invalid("more than one attachment starts with that; give more of its hash")),
    }
}

/// What one item of a send became.
enum Item {
    Image(trs_media::Prepared),
    File { path: PathBuf, name: String },
}

/// Sorts what a caller attaches into images and files: an image within the caps is an image (read, checked and its
/// metadata stripped); any other file, or an image over the caps, is a file. A folder or a missing file is refused.
fn classify(items: Vec<NewAttachment>) -> Result<Vec<Item>, String> {
    items
        .into_iter()
        .map(|i| match i {
            NewAttachment::Image(NewImage { name, bytes }) => {
                let shown = name.rsplit(['/', '\\']).next().filter(|n| !n.trim().is_empty()).unwrap_or("image").to_string();
                trs_media::prepare(&name, &bytes).map(Item::Image).map_err(|e| format!("{shown}: {e}"))
            }
            NewAttachment::Path(path) => {
                let name = safe_name(&path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default());
                let meta = std::fs::metadata(&path).map_err(|e| format!("{name}: {}", if trs_platform::fs::is_not_found(&e) { "there is no such file".to_string() } else { e.to_string() }))?;
                if meta.is_dir() {
                    return Err(folder_refused(&name));
                }
                if meta.len() <= trs_media::MAX_IMAGE as u64 {
                    let bytes = std::fs::read(&path).map_err(|e| format!("{name}: {e}"))?;
                    if trs_media::sniff(&bytes).is_some()
                        && let Ok(p) = trs_media::prepare(&name, &bytes)
                    {
                        return Ok(Item::Image(p));
                    }
                }
                Ok(Item::File { path, name })
            }
        })
        .collect()
}

impl SyncHandle {
    /// A channel's or a private conversation's attachments.
    pub(crate) fn room(&self, place: &Place) -> Result<Arc<dyn Room>, SyncError> {
        match place {
            Place::Channel(id) => Ok(self.channel(id)? as Arc<dyn Room>),
            Place::Direct(d) => self.direct_room(d),
        }
    }

    /// Posts a text message with images and files (SPEC §7G.7 *Attachments*), at most
    /// [`MAX_ATTACHMENTS`], to a channel (`topic` there) or a private conversation. An image is checked, its metadata
    /// stripped and capped ([`trs_media::prepare`]), and kept here sealed. A file of any size is read once for its
    /// hash and piece table (off the service's locks; meanwhile [`SyncHandle::chat_preparing`] shows it), then shared
    /// from where it is: never copied. The message is posted when that is done. The body is `caption` and, for devices
    /// without attachments, a line per attachment.
    #[allow(clippy::too_many_arguments)]
    pub async fn chat_send_attachments(
        &self,
        place: impl Into<Place>,
        topic: Option<&str>,
        caption: &str,
        reply_to: Option<&str>,
        agent: Option<&str>,
        items: Vec<NewAttachment>,
    ) -> Result<MessageInfo, SyncError> {
        let place = place.into();
        if items.is_empty() {
            return match place {
                Place::Channel(id) => self.chat_send(&id, topic, MessageKind::Text, caption, reply_to, agent).await,
                Place::Direct(d) => self.chat_send_direct(&d, MessageKind::Text, caption, reply_to, agent).await,
            };
        }
        if items.len() > MAX_ATTACHMENTS {
            return Err(invalid(format!("a message can have at most {MAX_ATTACHMENTS} attachments")));
        }
        // A private message goes only while its device is connected directly; when it is not, the send is still
        // shown (below) with that reason, so a caller that did not wait for the answer sees it.
        let ready = match &place {
            Place::Direct(d) => self.direct_ready(d),
            Place::Channel(_) => Ok(()),
        };
        let room = match self.room(&place) {
            Ok(r) => r,
            Err(e) => return Err(ready.err().unwrap_or(e)),
        };
        // Every send with attachments is shown as being prepared until it is posted (files are read first, images
        // checked), and one that fails stays there with its reason until dismissed: whoever sent it sees why, also
        // when it was sent without waiting for the answer (the app).
        let named: Vec<(String, u64)> = items
            .iter()
            .map(|i| match i {
                NewAttachment::Path(p) => (safe_name(&p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()), std::fs::metadata(p).map_or(0, |m| m.len())),
                NewAttachment::Image(im) => (trs_media::file_name(&im.name, trs_media::sniff(&im.bytes).unwrap_or(trs_media::Kind::Png)), im.bytes.len() as u64),
            })
            .collect();
        let (id, done) = room.files().preparing(named.iter().map(|n| n.0.clone()).collect(), named.iter().map(|n| n.1).sum());
        let out = match ready {
            Ok(()) => self.read_and_post(&place, &*room, topic, caption, reply_to, agent, items, done).await,
            Err(e) => Err(e),
        };
        room.files().prepared(id, out.as_ref().err().map(|e| e.to_string()));
        out
    }

    #[allow(clippy::too_many_arguments)]
    async fn read_and_post(
        &self,
        place: &Place,
        room: &dyn Room,
        topic: Option<&str>,
        caption: &str,
        reply_to: Option<&str>,
        agent: Option<&str>,
        items: Vec<NewAttachment>,
        done: Arc<std::sync::atomic::AtomicU64>,
    ) -> Result<MessageInfo, SyncError> {
        let items = tokio::task::spawn_blocking(move || classify(items)).await.map_err(io::Error::other)?.map_err(invalid)?;
        let paths: Vec<(PathBuf, String)> = items.iter().filter_map(|i| if let Item::File { path, name, .. } = i { Some((path.clone(), name.clone())) } else { None }).collect();
        // Images are ready once checked: their bytes count as done.
        let images: u64 = items.iter().map(|i| if let Item::Image(p) = i { p.bytes.len() as u64 } else { 0 }).sum();
        done.fetch_add(images, std::sync::atomic::Ordering::Relaxed);
        let prepared = tokio::task::spawn_blocking(move || {
            let stop = AtomicBool::new(false);
            paths.iter().map(|(p, name)| crate::files::prepare(p, &done, &stop).map_err(|e| format!("{name}: {e}"))).collect::<Result<Vec<_>, String>>()
        })
        .await
        .map_err(io::Error::other)?
        .map_err(invalid)?;
        self.post_attachments(place, room, topic, caption, reply_to, agent, items, &prepared).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn post_attachments(
        &self,
        place: &Place,
        room: &dyn Room,
        topic: Option<&str>,
        caption: &str,
        reply_to: Option<&str>,
        agent: Option<&str>,
        items: Vec<Item>,
        prepared: &[Prepared],
    ) -> Result<MessageInfo, SyncError> {
        let caption = caption.trim_end();
        let mut body = caption.to_string();
        let mut refs = Vec::new();
        let mut files = prepared.iter();
        let mut shares = Vec::new();
        for i in &items {
            if !body.is_empty() {
                body.push('\n');
            }
            match i {
                Item::Image(p) => {
                    body.push_str(&fallback_line(false, &p.name, p.bytes.len() as u64));
                    refs.push(AttachmentRef {
                        hash: crate::blobs::sha256(&p.bytes),
                        kind: p.kind.byte(),
                        size: p.bytes.len() as u64,
                        width: p.width,
                        height: p.height,
                        name: p.name.clone(),
                        thumb: p.thumb.clone(),
                        pieces: [0; 32],
                    });
                }
                Item::File { path, name, .. } => {
                    let f = files.next().ok_or_else(|| invalid("a file was not read"))?;
                    body.push_str(&fallback_line(true, name, f.size));
                    refs.push(AttachmentRef { hash: f.hash, kind: KIND_FILE, size: f.size, width: 0, height: 0, name: name.clone(), thumb: Vec::new(), pieces: f.table_hash });
                    shares.push((f, path));
                }
            }
        }
        let mut att = Attachments { caption_len: caption.len(), refs };
        // Previews while they fit; the attachments themselves always go.
        while att.to_extra().len() > MAX_EXTRA {
            let Some(r) = att.refs.iter_mut().rev().find(|r| !r.thumb.is_empty()) else { break };
            r.thumb.clear();
        }
        for (i, r) in items.iter().zip(&att.refs) {
            if let Item::Image(p) = i {
                room.blobs().put(&r.hash, &p.bytes)?;
                room.blobs().pin(r.hash);
            }
        }
        for (f, path) in shares {
            room.files().share(f, path, true)?;
        }
        match place {
            Place::Channel(id) => self.chat_post(id, topic, MessageKind::Text, &body, reply_to, agent, att.to_extra()).await,
            Place::Direct(d) => self.direct_post(d, MessageKind::Text, &body, reply_to, agent, att.to_extra()).await,
        }
    }

    /// Posts a text message with images to a channel (pictures' bytes; see [`SyncHandle::chat_send_attachments`]).
    pub async fn chat_send_images(&self, id: &trs_sync_proto::ids::FolderId, topic: Option<&str>, caption: &str, reply_to: Option<&str>, agent: Option<&str>, images: Vec<NewImage>) -> Result<MessageInfo, SyncError> {
        self.chat_send_attachments(id, topic, caption, reply_to, agent, images.into_iter().map(NewAttachment::Image).collect()).await
    }

    /// An attachment's hash and where it stands now.
    fn attachment_now(&self, room: &dyn Room, hash: &str) -> Result<([u8; 32], AttachmentInfo), SyncError> {
        let me = *self.inner.ctx.key.public_key();
        let mut out = Err(SyncError::NotFound);
        room.with_index(&self.inner.ctx, &mut |ix| {
            out = find(ix, hash).and_then(|h| ix.refs.get(&h).map(|n| (h, attachment_info(room, ix, &n.r, n.author == me))).ok_or(SyncError::NotFound));
        });
        out
    }

    /// An attachment of a channel's or conversation's messages by its hash (or the first 8 or more of its hex
    /// characters, when only one attachment starts with them): where it stands, and an image's bytes when here. An
    /// image not here is fetched (also one beyond the channel's budget); read again when its state is `available`. A
    /// file is never fetched by this: see [`SyncHandle::chat_file_download`].
    pub fn chat_attachment(&self, place: impl Into<Place>, hash: &str) -> Result<AttachmentBytes, SyncError> {
        let room = self.room(&place.into())?;
        let (h, info) = self.attachment_now(&*room, hash)?;
        if info.is_file() {
            return Ok((info, None));
        }
        if info.state == BlobState::Available.name() {
            return Ok((info, room.blobs().get(&h)));
        }
        if matches!(info.state.as_str(), "fetching" | "not_downloaded") {
            room.blobs().pin(h);
            crate::blobs::kick(&self.inner.ctx, &room);
        }
        Ok((self.attachment_now(&*room, &hex(&h))?.1, None))
    }

    /// Fetches an image now: one beyond the channel's budget, or one that failed (asked again from every member).
    pub fn chat_attachment_fetch(&self, place: impl Into<Place>, hash: &str) -> Result<AttachmentInfo, SyncError> {
        let room = self.room(&place.into())?;
        let (h, info) = self.attachment_now(&*room, hash)?;
        if info.is_file() {
            return Err(invalid("that is a file: download it"));
        }
        room.blobs().retry(&h);
        room.blobs().pin(h);
        crate::blobs::kick(&self.inner.ctx, &room);
        Ok(self.attachment_now(&*room, &hex(&h))?.1)
    }

    /// The small preview a message carries for an image (a JPEG from its author; empty: none). Check it before
    /// showing it ([`trs_media::thumb_png`]).
    pub fn chat_attachment_preview(&self, place: impl Into<Place>, hash: &str) -> Result<Vec<u8>, SyncError> {
        let place = place.into();
        let room = self.room(&place)?;
        let (h, _) = self.attachment_now(&*room, hash)?;
        Ok(match place {
            Place::Channel(id) => self.channel_preview(&id, &h),
            Place::Direct(d) => self.direct_preview(&d, &h),
        })
    }

    /// Downloads a file of a message (SPEC §7G.7 *Files*): to `to`, or into the Downloads folder. It runs while the
    /// device that has it is connected, pauses when none is, and resumes (also after a restart); one paused, gone or
    /// failed is tried again from where it was. Returns where it stands.
    pub fn chat_file_download(&self, place: impl Into<Place>, hash: &str, to: Option<DownloadTo>) -> Result<AttachmentInfo, SyncError> {
        let room = self.room(&place.into())?;
        let (h, info) = self.attachment_now(&*room, hash)?;
        if !info.is_file() {
            return Err(invalid("that is an image, not a file"));
        }
        let to = match to {
            Some(t) => t,
            None => DownloadTo::Folder(trs_platform::dirs::downloads().ok_or_else(|| invalid("there is no Downloads folder: choose where to save it"))?),
        };
        let n = crate::room::named(&self.inner.ctx, &*room, &h).ok_or(SyncError::NotFound)?;
        crate::files::download(&self.inner.ctx, &room, &n, to).map_err(invalid)?;
        Ok(self.attachment_now(&*room, &hex(&h))?.1)
    }

    /// Stops a download and deletes its partial file.
    pub fn chat_file_cancel(&self, place: impl Into<Place>, hash: &str) -> Result<AttachmentInfo, SyncError> {
        let room = self.room(&place.into())?;
        let (h, _) = self.attachment_now(&*room, hash)?;
        room.files().cancel(&h)?;
        Ok(self.attachment_now(&*room, &hex(&h))?.1)
    }

    /// Stops sharing a file sent from this device: those who ask for it hear that it is gone.
    pub fn chat_file_stop_sharing(&self, place: impl Into<Place>, hash: &str) -> Result<AttachmentInfo, SyncError> {
        let room = self.room(&place.into())?;
        let (h, _) = self.attachment_now(&*room, hash)?;
        if !room.files().stop_sharing(&h)? {
            return Err(invalid(concat!("that file is not shared from this ", trs_platform::computer!())));
        }
        Ok(self.attachment_now(&*room, &hex(&h))?.1)
    }

    /// The sends of a channel or conversation whose files are being read, or that failed (until dismissed).
    pub fn chat_preparing(&self, place: impl Into<Place>) -> Vec<PreparingInfo> {
        self.room(&place.into()).map(|r| r.files().preparing_list()).unwrap_or_default()
    }

    /// Forgets a send that failed.
    pub fn chat_dismiss_preparing(&self, place: impl Into<Place>, id: u64) -> bool {
        self.room(&place.into()).is_ok_and(|r| r.files().dismiss(id))
    }

    /// Starts every room's attachments that wait (the service started): images wanted, downloads asked for.
    pub(crate) fn wake_rooms(&self) {
        for c in self.inner.ctx.registry.channels() {
            let room: Arc<dyn Room> = c;
            crate::blobs::kick(&self.inner.ctx, &room);
            crate::files::resume(&self.inner.ctx, &room);
        }
        self.wake_direct_rooms();
    }

    /// Whether a private message to `device` can be written now (the reason when not).
    pub fn chat_direct_ready(&self, device: &trs_sync_proto::ids::DeviceId) -> Result<(), SyncError> {
        self.direct_ready(device)
    }

    /// The service stops: every download stops where it is.
    pub(crate) fn halt_rooms(&self) {
        for c in self.inner.ctx.registry.channels() {
            c.files.halt();
        }
        for r in crate::direct::rooms(&self.inner.ctx) {
            r.files().halt();
        }
    }

    /// **Tests:** what serving files does to the pieces it sends.
    #[doc(hidden)]
    pub fn test_file_hooks(&self) -> &crate::files::Hooks {
        &self.inner.ctx.file_hooks
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fallback_lines_say_image_or_file_and_the_size() {
        assert_eq!(fallback_line(false, "shot.png", 240_000), "[image: shot.png, 240 KB]");
        assert_eq!(fallback_line(true, "film.mov", 1_234_000_000), "[file: film.mov, 1.2 GB]");
        assert_eq!(folder_refused("Photos"), "Photos is a folder. Only files can be attached; to share a folder, use Folder Sync.");
    }

    #[test]
    fn attachments_are_sorted_into_images_and_files() {
        let dir = tempfile::tempdir().unwrap();
        let notes = dir.path().join("notes.txt");
        std::fs::write(&notes, b"plain text").unwrap();
        let mut png = Vec::new();
        image::DynamicImage::new_rgba8(4, 4).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
        let shot = dir.path().join("shot.png");
        std::fs::write(&shot, &png).unwrap();
        let fake = dir.path().join("broken.png");
        std::fs::write(&fake, b"\x89PNG\r\n\x1a\nnot really").unwrap();
        let got = classify(vec![NewAttachment::Path(notes), NewAttachment::Path(shot), NewAttachment::Path(fake)]).unwrap();
        assert!(matches!(&got[0], Item::File { name, .. } if name == "notes.txt"));
        assert!(matches!(&got[1], Item::Image(p) if p.name == "shot.png"));
        assert!(matches!(&got[2], Item::File { .. }), "an image that does not decode goes as a file");
        let e = classify(vec![NewAttachment::Path(dir.path().to_path_buf())]).err().unwrap();
        assert!(e.ends_with("is a folder. Only files can be attached; to share a folder, use Folder Sync."), "{e}");
        let e = classify(vec![NewAttachment::Path(dir.path().join("nope.bin"))]).err().unwrap();
        assert_eq!(e, "nope.bin: there is no such file");
        let e = classify(vec![NewAttachment::Image(NewImage { name: "x.png".into(), bytes: b"text".to_vec() })]).err().unwrap();
        assert!(e.starts_with("x.png: "), "{e}");
    }
}
