//! Images in channel messages between services on loopback (SPEC §7G.7 *Attachments*): a member fetches an image by
//! its hash from the device that has it, an image whose author is away waits with its preview until a member that has
//! it comes back, and a device without images passes the message on intact.

use std::io::Cursor;

use trs_sync_net::chat::{MessageKind, NewImage};
use trs_sync_net::{Options, SyncHandle};
use trs_sync_proto::ids::FolderId;
use trs_sync_proto::members::Role;

use crate::chat::{at, join, start_at};
use crate::utp::{device_with, until};

/// A PNG of `w`×`h` with a text chunk that says where it was taken.
fn png_with_location(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbaImage::from_fn(w, h, |x, y| image::Rgba([(x * 3) as u8, (y * 5) as u8, 90, 255]));
    let mut png = Vec::new();
    image::DynamicImage::ImageRgba8(img).write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
    // After IHDR (8 + 25 bytes): a tEXt chunk with a place in it.
    let data = b"Comment\0taken at 59.9139N 10.7522E";
    let mut chunk = (data.len() as u32).to_be_bytes().to_vec();
    chunk.extend_from_slice(b"tEXt");
    chunk.extend_from_slice(data);
    let mut crc = !0u32;
    for b in b"tEXt".iter().chain(data.iter()) {
        crc ^= u32::from(*b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { 0xEDB8_8320 ^ (crc >> 1) } else { crc >> 1 };
        }
    }
    chunk.extend_from_slice(&(!crc).to_be_bytes());
    [&png[..33], &chunk, &png[33..]].concat()
}

fn has(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// The state of the first image of the newest message.
fn state(h: &SyncHandle, id: &FolderId) -> String {
    h.chat_read(id, None, None, Some(1)).ok().and_then(|m| m.first().and_then(|m| m.attachments.first().map(|a| a.state.clone()))).unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_member_fetches_an_image_by_its_hash_and_checks_it() {
    let dirs: Vec<tempfile::TempDir> = (0..2).map(|_| tempfile::tempdir().unwrap()).collect();
    let a = start_at(dirs[0].path(), "Anna", |_| {}).await;
    let b = start_at(dirs[1].path(), "Bo", |_| {}).await;
    let id: FolderId = a.chat_new("Design", false).await.unwrap().id.parse().unwrap();
    join(&b, &a, &id, Role::ReadWrite, at(&a)).await;

    let shot = png_with_location(300, 120);
    let sent = a.chat_send_images(&id, None, "the new header", None, None, vec![NewImage { name: "/Users/anna/Desktop/Screenshot 1.png".into(), bytes: shot }]).await.unwrap();
    assert_eq!((sent.kind.as_str(), sent.body.as_str()), ("text", "the new header"), "the caption alone");
    let att = sent.attachments[0].clone();
    assert_eq!((att.kind.as_str(), att.name.as_str(), att.width, att.height, att.state.as_str()), ("png", "Screenshot 1.png", 300, 120, "available"));
    let (_, here) = a.chat_attachment(id, &att.hash).unwrap();
    let here = here.unwrap();
    assert!(!has(&here, b"59.9139N"), "the place it was taken does not leave the device");
    assert_eq!(trs_sync_net::blobs::sha256(&here), trs_sync_net::blobs::hash_from_hex(&att.hash).unwrap());

    until(30, "B has the image", || state(&b, &id) == "available").await;
    let got = b.chat_read(&id, None, None, Some(1)).unwrap().remove(0);
    assert_eq!((got.body.as_str(), got.attachments[0].hash.as_str(), got.trust.as_str()), ("the new header", att.hash.as_str(), "untrusted-peer"));
    let (info, bytes) = b.chat_attachment(id, &att.hash[..12]).unwrap();
    assert_eq!((info.size, bytes.as_deref()), (here.len() as u64, Some(&*here)), "the same bytes, by a prefix of the hash");
    // The preview came with the message; checked and made a PNG for the host.
    let preview = b.chat_attachment_preview(id, &att.hash).unwrap();
    let thumb = trs_media::thumb_png(&preview).unwrap();
    assert_eq!((thumb.mime, thumb.width, thumb.height), ("image/png", 160, 64));
    // Kept sealed in the channel's own container, which goes with the channel.
    let blobs = dirs[1].path().join("sync").join(format!("{id}.blobs"));
    let on_disk: Vec<u8> = std::fs::read_dir(&blobs).unwrap().flatten().flat_map(|e| std::fs::read(e.path()).unwrap()).collect();
    assert!(!on_disk.is_empty() && !has(&on_disk, b"IHDR"), "sealed at rest");
    b.chat_leave(&id).unwrap();
    assert!(!blobs.exists(), "deleted with the channel");
    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_image_waits_for_its_author_and_a_device_without_images_passes_it_on() {
    let dirs: Vec<tempfile::TempDir> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let a = start_at(dirs[0].path(), "Anna", |_| {}).await;
    // Cy runs a Seedwell without images (0.1.14): it keeps and passes on messages, never images. It admits Bo.
    let old = |o: &mut Options| o.features &= !trs_sync_proto::msg::FEATURE_BLOBS;
    let c = start_at(dirs[2].path(), "Cy", old).await;
    let id: FolderId = a.chat_new("Team", false).await.unwrap().id.parse().unwrap();
    join(&c, &a, &id, Role::Admin, at(&a)).await;

    let sent = a.chat_send_images(&id, None, "", None, None, vec![NewImage { name: "plan.png".into(), bytes: png_with_location(64, 64) }]).await.unwrap();
    let hash = sent.attachments[0].hash.clone();
    until(30, "Cy has the message", || c.chat_read(&id, None, None, None).is_ok_and(|m| m.len() == 1)).await;
    a.shutdown().await;
    // Bo joins while Anna is away, admitted by Cy: through Cy (which has no images) come the message, its preview,
    // and a placeholder.
    let b = start_at(dirs[1].path(), "Bo", |_| {}).await;
    join(&b, &c, &id, Role::ReadWrite, at(&c)).await;
    until(30, "Bo has the message", || b.chat_read(&id, None, None, None).is_ok_and(|m| m.len() == 1)).await;
    let m = b.chat_read(&id, None, None, None).unwrap().remove(0);
    assert_eq!((m.body.as_str(), m.attachments[0].state.as_str()), ("", "fetching"), "waits for a member that has it");
    assert!(!b.chat_attachment_preview(id, &hash).unwrap().is_empty(), "its preview is shown meanwhile");
    assert!(b.chat_attachment(id, &hash).unwrap().1.is_none());
    // The text a Seedwell without images shows.
    let raw = c.chat_read(&id, None, None, None).unwrap().remove(0);
    assert_eq!(raw.attachments.len(), 1, "Cy kept the images record as it came");

    // Anna is back: Bo fetches it from her.
    let a = start_at(dirs[0].path(), "Anna", |_| {}).await;
    b.add_address(&id, at(&a)).unwrap();
    until(60, "Bo has the image", || state(&b, &id) == "available").await;
    assert_eq!(b.chat_attachment(id, &hash).unwrap().1.map(|v| v.len() as u64), Some(sent.attachments[0].size));
    for h in [a, b, c] {
        h.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn images_are_refused_by_their_bytes_and_counted() {
    let a = device_with("Anna", |_| {}).await;
    let id: FolderId = a.h.chat_new("Solo", false).await.unwrap().id.parse().unwrap();
    let text = NewImage { name: "notes.png".into(), bytes: b"not an image at all".to_vec() };
    let e = a.h.chat_send_images(&id, None, "x", None, None, vec![text]).await.unwrap_err().to_string();
    assert!(e.contains("notes.png") && e.contains("PNG, JPEG, GIF, WebP and SVG"), "{e}");
    let many: Vec<NewImage> = (0..11).map(|_| NewImage { name: "a.png".into(), bytes: png_with_location(2, 2) }).collect();
    assert!(a.h.chat_send_images(&id, None, "", None, None, many).await.unwrap_err().to_string().contains("at most 10"));
    assert!(a.h.chat_read(&id, None, None, None).unwrap().is_empty(), "nothing posted");
    // A plain text message is unchanged.
    let t = a.h.chat_send(&id, None, MessageKind::Text, "[image: fake.png, 1 KB]", None, None).await.unwrap();
    assert!(t.attachments.is_empty() && t.body == "[image: fake.png, 1 KB]");
    a.h.shutdown().await;
}
