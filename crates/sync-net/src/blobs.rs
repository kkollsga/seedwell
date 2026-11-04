//! Images of a channel's or a private conversation's messages (SPEC §7G.7 *Attachments*): blobs named by their SHA-256,
//! kept sealed with the room's own key in its container (`sync/<id>.blobs/`, `sync/dm-<device>.blobs/`, deleted with
//! it), and fetched from the devices that hold them ([`crate::room`]). Files are [`crate::files`]'.
//!
//! * **What is fetched by itself**: the images the kept messages name, newest first, up to [`CHANNEL_BUDGET`] bytes a
//!   channel (every image of a private conversation); older ones are "not downloaded" (and their bytes deleted here),
//!   fetched when asked for ([`crate::service::SyncHandle::chat_attachment_fetch`]). An image over its kind's cap is
//!   never fetched.
//! * **From whom**: any connected member whose session has [`trs_sync_proto::msg::FEATURE_BLOBS`] (a private
//!   conversation's other device only, over a direct session), one after another, in parts of [`PART`] bytes. The
//!   bytes are checked against the hash, then decoded ([`trs_media::check`]): a peer that sends other bytes is passed
//!   over, an image that does not decode is marked failed for good.
//! * **When**: when messages with images arrive, when a member connects, and when asked. With nobody to fetch from
//!   (the author offline), the image waits (its message shows its preview) until a member that has it connects.
//! * **Serving**: a member asks for a part of an image a kept message names; it is answered from the sealed copy
//!   (opened once and kept in memory while it is being served). A file's piece table or piece is answered by
//!   [`crate::files::serve`].
//!
//! A blob file is `seal(bytes)` ([`crate::at_rest`], associated data `SWBL`), written whole and renamed into place.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use trs_sync_proto::members::hex;
use trs_sync_proto::msg::{BlobCode, BlobData, BlobWant};

use crate::at_rest::Sealer;
use crate::room::{Room, named};
use crate::shared::{Ctx, PeerHandle, lock};

/// Bytes of images a channel keeps by itself on each device (the newest messages' first).
pub const CHANNEL_BUDGET: u64 = 512 << 20;
/// Bytes asked for at a time.
pub const PART: u32 = 1 << 20;
/// How long one part may take.
const PART_TIME: Duration = Duration::from_secs(30);
/// The associated data of a sealed blob.
pub(crate) const AAD: &[u8] = b"SWBL";

/// SHA-256 of `b`.
pub fn sha256(b: &[u8]) -> [u8; 32] {
    ring::digest::digest(&ring::digest::SHA256, b).as_ref().try_into().expect("32 bytes")
}

/// Where an image of a message stands on this device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlobState {
    /// Here.
    Available,
    /// Wanted: being fetched, or waiting for a member that has it.
    Fetching,
    /// Beyond the channel's budget: fetched only when asked for.
    NotDownloaded,
    /// Over its kind's cap: never fetched.
    TooLarge,
    /// It cannot be shown (why).
    Failed(String),
}

impl BlobState {
    /// Its name for agents: `available`, `fetching`, `not_downloaded`, `too_large` or `failed`.
    pub fn name(&self) -> &'static str {
        match self {
            BlobState::Available => "available",
            BlobState::Fetching => "fetching",
            BlobState::NotDownloaded => "not_downloaded",
            BlobState::TooLarge => "too_large",
            BlobState::Failed(_) => "failed",
        }
    }
}

#[derive(Default)]
struct State {
    /// Images that cannot be shown, and why.
    failed: HashMap<[u8; 32], String>,
    /// Images kept beyond the budget until the next start: asked for, or sent from here.
    pinned: HashSet<[u8; 32]>,
    /// A fetcher runs; `again`: something new came while it ran.
    running: bool,
    again: bool,
    /// The image being served (opened once for all its parts).
    serving: Option<([u8; 32], Arc<Vec<u8>>)>,
}

/// A channel's images on this device.
pub(crate) struct Blobs {
    dir: PathBuf,
    seal: Sealer,
    state: Mutex<State>,
}

impl Blobs {
    pub(crate) fn new(dir: PathBuf, seal: Sealer) -> Blobs {
        Blobs { dir, seal, state: Mutex::new(State::default()) }
    }

    /// The folder of a room's images, next to its messages file (`base`: `<channel id>` or `dm-<device>`).
    pub(crate) fn dir_of(state_dir: &Path, base: &str) -> PathBuf {
        state_dir.join(format!("{base}.blobs"))
    }

    fn path(&self, hash: &[u8; 32]) -> PathBuf {
        self.dir.join(hex(hash))
    }

    /// The image is here.
    pub(crate) fn has(&self, hash: &[u8; 32]) -> bool {
        self.path(hash).is_file()
    }

    /// Keeps an image (its hash checked by the caller), sealed.
    pub(crate) fn put(&self, hash: &[u8; 32], bytes: &[u8]) -> io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let tmp = self.dir.join(format!(".{}.tmp", hex(hash)));
        std::fs::write(&tmp, self.seal.seal(bytes))?;
        crate::shared::slow_disk();
        std::fs::rename(&tmp, self.path(hash))?;
        lock(&self.state).failed.remove(hash);
        Ok(())
    }

    /// An image's bytes, if it is here and opens.
    pub(crate) fn get(&self, hash: &[u8; 32]) -> Option<Arc<Vec<u8>>> {
        if let Some((h, b)) = lock(&self.state).serving.as_ref()
            && h == hash
        {
            return Some(b.clone());
        }
        let sealed = std::fs::read(self.path(hash)).ok()?;
        let plain = self.seal.open(&sealed)?;
        (sha256(&plain) == *hash).then(|| Arc::new(plain))
    }

    /// Kept until the next start, whatever the budget.
    pub(crate) fn pin(&self, hash: [u8; 32]) {
        lock(&self.state).pinned.insert(hash);
    }

    /// Forgets that an image failed, so it is fetched again.
    pub(crate) fn retry(&self, hash: &[u8; 32]) {
        lock(&self.state).failed.remove(hash);
    }

    pub(crate) fn failed(&self, hash: &[u8; 32]) -> Option<String> {
        lock(&self.state).failed.get(hash).cloned()
    }

    fn pinned(&self) -> HashSet<[u8; 32]> {
        lock(&self.state).pinned.clone()
    }

    /// Deletes the images that are neither within `keep` nor pinned.
    pub(crate) fn sweep(&self, keep: &HashSet<[u8; 32]>) {
        let pinned = self.pinned();
        let Ok(entries) = std::fs::read_dir(&self.dir) else { return };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let Some(h) = hash_from_hex(&name) else { continue };
            if !keep.contains(&h) && !pinned.contains(&h) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

/// A hash from its 64 hex characters.
pub fn hash_from_hex(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

/// Where an image a kept message names stands here.
pub(crate) fn state_of(blobs: &Blobs, kind: u8, size: u64, hash: &[u8; 32], in_budget: bool) -> BlobState {
    let Some(k) = trs_media::Kind::from_byte(kind) else {
        return BlobState::Failed("an image of a kind this version cannot show".into());
    };
    if size > k.max_bytes() as u64 {
        return BlobState::TooLarge;
    }
    if blobs.has(hash) {
        return BlobState::Available;
    }
    if let Some(why) = blobs.failed(hash) {
        return BlobState::Failed(why);
    }
    if in_budget || lock(&blobs.state).pinned.contains(hash) { BlobState::Fetching } else { BlobState::NotDownloaded }
}

/// Starts fetching what the room wants, unless a fetcher runs (it then looks again when done).
pub(crate) fn kick(ctx: &Arc<Ctx>, c: &Arc<dyn Room>) {
    {
        let mut s = lock(&c.blobs().state);
        if s.running {
            s.again = true;
            return;
        }
        s.running = true;
        s.again = false;
    }
    let (ctx, c) = (ctx.clone(), c.clone());
    tokio::spawn(async move {
        loop {
            fetch_all(&ctx, &c).await;
            let mut s = lock(&c.blobs().state);
            if !s.again {
                s.running = false;
                break;
            }
            s.again = false;
        }
    });
}

/// What the room wants that is not here: (hash, kind, size), newest first.
fn wanted(ctx: &Ctx, c: &dyn Room) -> Vec<([u8; 32], trs_media::Kind, u64)> {
    let pinned = c.blobs().pinned();
    let mut all = Vec::new();
    c.with_index(ctx, &mut |ix| all = ix.wanted_images(&pinned));
    all.retain(|(h, _, _)| !c.blobs().has(h) && c.blobs().failed(h).is_none());
    all
}

/// The images to keep: those within the budget.
fn keep(ctx: &Ctx, c: &dyn Room) -> HashSet<[u8; 32]> {
    let mut keep = HashSet::new();
    c.with_index(ctx, &mut |ix| keep = ix.budget_set.clone());
    keep
}

/// Fetches what is wanted from the connected members, until it is all here or nobody has the rest.
async fn fetch_all(ctx: &Arc<Ctx>, c: &Arc<dyn Room>) {
    loop {
        let want = wanted(ctx, &**c);
        let peers = c.peers(ctx);
        if want.is_empty() || peers.is_empty() {
            break;
        }
        let mut progress = false;
        for (hash, kind, size) in want {
            for p in &peers {
                let Some(bytes) = fetch_one(p, &**c, &hash, size).await else { continue };
                // The bytes it is named by; then they must be the image the message says.
                if bytes.len() as u64 != size || sha256(&bytes) != hash {
                    continue;
                }
                let checked = tokio::task::spawn_blocking(move || trs_media::check(kind, &bytes).map(|_| bytes)).await;
                match checked {
                    Ok(Ok(bytes)) => {
                        if let Err(e) = c.blobs().put(&hash, &bytes) {
                            c.problem(format!("the images could not be saved: {e}"));
                            return;
                        }
                    }
                    Ok(Err(e)) => {
                        lock(&c.blobs().state).failed.insert(hash, e.to_string());
                    }
                    Err(_) => {}
                }
                progress = true;
                break;
            }
        }
        if !progress {
            break;
        }
    }
    c.blobs().sweep(&keep(ctx, &**c));
}

/// The whole image from one peer, part by part, or `None` (it does not have it, it went away, it took too long).
async fn fetch_one(p: &PeerHandle, c: &dyn Room, hash: &[u8; 32], size: u64) -> Option<Vec<u8>> {
    fetch_coded(p, c, hash, size).await.ok()
}

/// A whole blob (an image, a file's piece table) from one peer, part by part; or the code it answered (`None`: it went
/// away, it took too long, it sent a part that does not fit). Grows as the parts come, never past `size`.
pub(crate) async fn fetch_coded(p: &PeerHandle, c: &dyn Room, hash: &[u8; 32], size: u64) -> Result<Vec<u8>, Option<BlobCode>> {
    let mut out = Vec::new();
    while (out.len() as u64) < size {
        let len = (size - out.len() as u64).min(u64::from(PART)) as u32;
        let want = BlobWant { id: 0, scope: c.scope(), hash: *hash, offset: out.len() as u64, len };
        let (id, rx) = p.ask_blob(want).await.ok_or(None)?;
        let got = tokio::time::timeout(PART_TIME, rx).await;
        let Ok(Ok(d)) = got else {
            p.forget_blob(id);
            return Err(None);
        };
        if d.code != BlobCode::Ok {
            return Err(Some(d.code));
        }
        if d.total != size || d.data.len() != len as usize {
            return Err(None);
        }
        out.extend_from_slice(&d.data);
    }
    Ok(out)
}

/// A member asks for part of an image: answered when a kept message names it and it is here. A file's piece table or
/// piece goes to [`crate::files::serve`].
pub(crate) fn serve(ctx: &Ctx, c: &dyn Room, w: &BlobWant) -> BlobData {
    let no = |code| BlobData { id: w.id, code, total: 0, data: Vec::new() };
    if w.len as usize > trs_sync_proto::msg::MAX_BLOB_CHUNK {
        return no(BlobCode::Refused);
    }
    match named(ctx, c, &w.hash) {
        Some(n) if n.r.is_file() => return crate::files::serve(ctx, c, w),
        Some(_) => {}
        None if crate::room::file_of_table(ctx, c, &w.hash).is_some() => return crate::files::serve(ctx, c, w),
        None => return no(BlobCode::Refused),
    }
    let Some(bytes) = c.blobs().get(&w.hash) else { return no(BlobCode::Missing) };
    lock(&c.blobs().state).serving = Some((w.hash, bytes.clone()));
    let total = bytes.len() as u64;
    if w.offset > total {
        return no(BlobCode::Refused);
    }
    let start = w.offset as usize;
    let end = (start + w.len as usize).min(bytes.len());
    if end == bytes.len() {
        // The last part: no need to keep it open.
        lock(&c.blobs().state).serving = None;
    }
    BlobData { id: w.id, code: BlobCode::Ok, total, data: bytes[start..end].to_vec() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blobs_are_sealed_on_disk_and_open_only_with_their_key() {
        let dir = tempfile::tempdir().unwrap();
        let b = Blobs::new(dir.path().join("x.blobs"), Sealer::new([7; 32], AAD));
        let bytes = b"\x89PNG\r\n\x1a\nsecret pixels of a screenshot".to_vec();
        let h = sha256(&bytes);
        assert!(!b.has(&h));
        b.put(&h, &bytes).unwrap();
        assert!(b.has(&h));
        let on_disk = std::fs::read(dir.path().join("x.blobs").join(hex(&h))).unwrap();
        assert!(!on_disk.windows(6).any(|w| w == b"secret"), "sealed at rest");
        assert_eq!(b.get(&h).as_deref(), Some(&bytes));
        // Another key (the channel's key gone): nothing opens.
        let other = Blobs::new(dir.path().join("x.blobs"), Sealer::new([8; 32], AAD));
        assert_eq!(other.get(&h), None);
        // Another file's associated data: nothing opens either.
        let chat = Blobs::new(dir.path().join("x.blobs"), Sealer::new([7; 32], b"SWCH"));
        assert_eq!(chat.get(&h), None);
        // Swept unless kept or pinned.
        b.sweep(&HashSet::new());
        assert!(!b.has(&h));
        b.put(&h, &bytes).unwrap();
        b.pin(h);
        b.sweep(&HashSet::new());
        assert!(b.has(&h));
        assert_eq!(hash_from_hex(&hex(&h)), Some(h));
        assert_eq!(hash_from_hex("zz"), None);
    }
}
