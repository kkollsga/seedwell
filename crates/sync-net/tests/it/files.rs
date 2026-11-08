//! Files in chat messages between services on loopback (SPEC §7G.7 *Files*): the sender serves the original in place,
//! nothing downloads by itself, a download checks each piece (a bad one is fetched again alone), resumes after a
//! restart, finds the file gone when its sender changed it, and fetches from another member that has it; a private
//! message's file goes only between its two devices.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use seedwell_sync_net::chat::{AttachmentInfo, MessageInfo, NewAttachment};
use seedwell_sync_net::files::DownloadTo;
use seedwell_sync_net::room::Place;
use seedwell_sync_net::SyncHandle;
use seedwell_sync_proto::attach::PIECE;
use seedwell_sync_proto::ids::FolderId;
use seedwell_sync_proto::members::Role;

use crate::chat::{at, join, start_at};
use crate::utp::until;

/// A file of `len` bytes that differ from piece to piece.
fn write_file(path: &Path, len: u64, seed: u8) -> Vec<u8> {
    let bytes: Vec<u8> = (0..len).map(|i| ((i * 31 + u64::from(seed) * 7 + i / 4093) % 251) as u8).collect();
    std::fs::write(path, &bytes).unwrap();
    bytes
}

/// The first attachment of the newest message of a channel.
fn first(h: &SyncHandle, id: &FolderId) -> Option<AttachmentInfo> {
    h.chat_read(id, None, None, Some(1)).ok()?.first()?.attachments.first().cloned()
}

fn state(h: &SyncHandle, id: &FolderId) -> String {
    first(h, id).map(|a| a.state).unwrap_or_default()
}

/// Bytes under a folder, all files together.
fn bytes_under(dir: &Path) -> u64 {
    std::fs::read_dir(dir).map(|r| r.flatten().map(|e| e.metadata().map(|m| m.len()).unwrap_or(0)).sum()).unwrap_or(0)
}

async fn send(h: &SyncHandle, place: impl Into<Place>, caption: &str, path: &Path) -> MessageInfo {
    h.chat_send_attachments(place, None, caption, None, None, vec![NewAttachment::Path(path.to_path_buf())]).await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_is_served_in_place_checked_piece_by_piece_and_found_gone_when_changed() {
    let dirs: Vec<tempfile::TempDir> = (0..4).map(|_| tempfile::tempdir().unwrap()).collect();
    let a = start_at(dirs[0].path(), "Anna", |_| {}).await;
    let b = start_at(dirs[1].path(), "Bo", |_| {}).await;
    let id: FolderId = a.chat_new("Data", false).await.unwrap().id.parse().unwrap();
    join(&b, &a, &id, Role::Admin, at(&a)).await;

    // Anna sends a file of five and a half pieces: shared from where it is, nothing copied.
    let src = dirs[3].path().join("report.bin");
    let bytes = write_file(&src, 5 * PIECE + PIECE / 2, 1);
    let sent = send(&a, id, "the numbers", &src).await;
    let att = sent.attachments[0].clone();
    assert_eq!((sent.body.as_str(), att.kind.as_str(), att.name.as_str(), att.size), ("the numbers", "file", "report.bin", bytes.len() as u64));
    assert_eq!((att.state.as_str(), att.path.as_deref()), ("shared", Some(src.to_str().unwrap())));
    assert!(a.chat_preparing(id).is_empty(), "posted once read");
    let kept = dirs[0].path().join("sync").join(format!("{id}.files"));
    assert!(bytes_under(&kept) < 4096, "only the share and its piece table are kept, sealed: {}", bytes_under(&kept));

    // Bo gets the message; nothing downloads by itself.
    until(30, "Bo has the message", || state(&b, &id) == "available").await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert_eq!((state(&b, &id).as_str(), a.test_file_hooks().served.load(Ordering::SeqCst)), ("available", 0));

    // He downloads it; one piece arrives damaged and is fetched again alone.
    a.test_file_hooks().corrupt.store(1, Ordering::SeqCst);
    let dl = dirs[1].path().join("Downloads");
    std::fs::create_dir_all(&dl).unwrap();
    std::fs::write(dl.join("report.bin"), b"already here").unwrap();
    b.chat_file_download(id, &att.hash[..12], Some(DownloadTo::Folder(dl.clone()))).unwrap();
    until(60, "Bo has the file", || state(&b, &id) == "downloaded").await;
    let got = first(&b, &id).unwrap();
    let path = PathBuf::from(got.path.clone().unwrap());
    assert_eq!(path, dl.join("report (1).bin"), "a free name beside the one there");
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    assert_eq!(a.test_file_hooks().served.load(Ordering::SeqCst), 7, "six pieces, and the bad one again");
    assert!(!dl.join("report (1).bin.seedwell-part").exists());
    assert_eq!(seedwell_platform::fs::is_marked_downloaded(&path), cfg!(any(target_os = "macos", windows)), "marked as downloaded");

    // Anna stops sharing it; Cy, who joins later, gets it from Bo's copy.
    let stopped = a.chat_file_stop_sharing(id, &att.hash).unwrap();
    assert_eq!((stopped.state.as_str(), stopped.problem.as_deref()), ("unshared", Some("you stopped sharing it")));
    let c = start_at(dirs[2].path(), "Cy", |_| {}).await;
    join(&c, &a, &id, Role::ReadWrite, at(&a)).await;
    c.add_address(&id, at(&b)).unwrap();
    until(30, "Cy has the message", || state(&c, &id) == "available").await;
    until(30, "Cy reaches Bo", || c.channel_status(&id).is_some_and(|s| s.online >= 3)).await;
    let cdl = dirs[2].path().join("in");
    c.chat_file_download(id, &att.hash, Some(DownloadTo::Folder(cdl.clone()))).unwrap();
    until(60, "Cy has the file", || state(&c, &id) == "downloaded").await;
    assert_eq!(std::fs::read(cdl.join("report.bin")).unwrap(), bytes);

    // A file its sender changed after sending it is gone: Anna's card says so, and Bo's download finds it gone.
    let src2 = dirs[3].path().join("draft.bin");
    write_file(&src2, 2 * PIECE + 7, 2);
    let sent2 = send(&a, id, "", &src2).await;
    let h2 = sent2.attachments[0].hash.clone();
    until(30, "Bo has the second message", || first(&b, &id).is_some_and(|x| x.hash == h2)).await;
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    write_file(&src2, 2 * PIECE + 7, 3);
    let mine = a.chat_attachment(id, &h2).unwrap().0;
    assert_eq!((mine.state.as_str(), mine.problem.as_deref()), ("unshared", Some("the file changed since it was sent")));
    b.chat_file_download(id, &h2, Some(DownloadTo::Folder(dl.clone()))).unwrap();
    until(30, "gone", || state(&b, &id) == "gone").await;
    assert!(!dl.join("draft.bin.seedwell-part").exists(), "nothing left behind");

    // Leaving the channel deletes its files' records; the downloaded file is Bo's and stays.
    b.chat_leave(&id).unwrap();
    assert!(!dirs[1].path().join("sync").join(format!("{id}.files")).exists());
    assert!(path.exists());
    for h in [a, b, c] {
        h.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_download_resumes_after_a_restart_from_the_pieces_it_has() {
    let dirs: Vec<tempfile::TempDir> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let a = start_at(dirs[0].path(), "Anna", |_| {}).await;
    let b = start_at(dirs[1].path(), "Bo", |_| {}).await;
    let id: FolderId = a.chat_new("Big", false).await.unwrap().id.parse().unwrap();
    join(&b, &a, &id, Role::ReadWrite, at(&a)).await;
    let src = dirs[2].path().join("big.bin");
    let pieces = 24u64;
    let bytes = write_file(&src, pieces * PIECE - 100, 5);
    let sent = send(&a, id, "", &src).await;
    let hash = sent.attachments[0].hash.clone();
    until(30, "Bo has the message", || state(&b, &id) == "available").await;

    // Slow pieces, so the download is cut off in the middle.
    a.test_file_hooks().delay_ms.store(300, Ordering::SeqCst);
    let dl = dirs[1].path().join("dl");
    b.chat_file_download(id, &hash, Some(DownloadTo::Folder(dl.clone()))).unwrap();
    until(30, "a third of it", || first(&b, &id).is_some_and(|x| x.state == "downloading" && x.done >= 8 * PIECE)).await;
    assert!(dl.join("big.bin.seedwell-part").exists(), "a partial file next to where it goes");
    b.shutdown().await;
    drop(b);
    let before = a.test_file_hooks().served.load(Ordering::SeqCst);
    a.test_file_hooks().delay_ms.store(0, Ordering::SeqCst);

    // Started again: it resumes by itself once Anna is reached, from what it had.
    let b = start_at(dirs[1].path(), "Bo", |_| {}).await;
    let had = first(&b, &id).unwrap();
    assert!(had.done >= 4 * PIECE && matches!(had.state.as_str(), "paused" | "downloading"), "{had:?}");
    b.add_address(&id, at(&a)).unwrap();
    until(60, "Bo has the file", || state(&b, &id) == "downloaded").await;
    assert_eq!(std::fs::read(dl.join("big.bin")).unwrap(), bytes);
    let after = a.test_file_hooks().served.load(Ordering::SeqCst);
    assert!(after - before < pieces, "the pieces it had are not fetched again: {} of {pieces}", after - before);
    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_private_messages_file_goes_only_between_its_two_devices() {
    let dirs: Vec<tempfile::TempDir> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let a = start_at(dirs[0].path(), "Anna", |_| {}).await;
    let b = start_at(dirs[1].path(), "Bo", |_| {}).await;
    let (ad, bd) = (a.device_id(), b.device_id());
    let id: FolderId = a.chat_new("Team", false).await.unwrap().id.parse().unwrap();
    join(&b, &a, &id, Role::ReadWrite, at(&a)).await;
    until(30, "the keys", || a.test_direct_knows(&bd) && b.test_direct_knows(&ad)).await;

    let src = dirs[2].path().join("contract.pdf");
    let bytes = write_file(&src, 3 * PIECE + 1, 9);
    let sent = send(&a, Place::Direct(bd), "for your eyes", &src).await;
    let att = sent.attachments[0].clone();
    assert_eq!((sent.direct, sent.place.clone(), att.state.as_str()), (true, format!("dm:{bd}"), "shared"));
    until(30, "Bo has it", || b.chat_direct_messages(&ad, None, 10).is_ok_and(|m| m.len() == 1)).await;
    let m = b.chat_direct_messages(&ad, None, 10).unwrap().remove(0);
    assert_eq!((m.body.as_str(), m.place.clone(), m.attachments[0].state.as_str()), ("for your eyes", format!("dm:{ad}"), "available"));
    // The file's name travels sealed: the channel's members see nothing of it.
    assert!(b.chat_read(&id, None, None, None).unwrap().iter().all(|m| m.attachments.is_empty()));

    let dl = dirs[1].path().join("dl");
    let e = b.chat_file_download(Place::Direct(ad), &att.hash, Some(DownloadTo::File(dl.join("mine.pdf")))).unwrap_err();
    assert!(e.to_string().contains("there is no folder"), "{e}");
    std::fs::create_dir_all(&dl).unwrap();
    b.chat_file_download(Place::Direct(ad), &att.hash, Some(DownloadTo::File(dl.join("mine.pdf")))).unwrap();
    until(60, "Bo has the file", || b.chat_direct_messages(&ad, None, 10).is_ok_and(|m| m[0].attachments[0].state == "downloaded")).await;
    assert_eq!(std::fs::read(dl.join("mine.pdf")).unwrap(), bytes, "saved where he chose");

    // An image in a private message is fetched by itself from the other device, as in a channel.
    let shot = dirs[2].path().join("shot.png");
    let mut png = Vec::new();
    image::DynamicImage::new_rgba8(8, 4).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
    std::fs::write(&shot, &png).unwrap();
    let pic = send(&a, Place::Direct(bd), "", &shot).await;
    assert_eq!((pic.attachments[0].kind.as_str(), pic.attachments[0].width), ("png", 8));
    let pic_hash = pic.attachments[0].hash.clone();
    until(30, "Bo has the picture", || b.chat_attachment(Place::Direct(ad), &pic_hash).is_ok_and(|(_, bytes)| bytes.is_some())).await;

    // Deleting the conversation deletes its attachments' records.
    let kept = dirs[0].path().join("sync").join(format!("dm-{bd}.files"));
    assert!(kept.exists());
    a.chat_delete_direct(&bd).unwrap();
    assert!(!kept.exists());
    a.shutdown().await;
    b.shutdown().await;
}

/// A send that fails after its caller stopped waiting (the app posts without waiting) is shown with its reason until
/// dismissed: pasted pictures alone too, and a private message whose device went away.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_send_that_fails_later_stays_shown_with_its_reason() {
    use seedwell_sync_net::chat::NewImage;
    let dirs: Vec<tempfile::TempDir> = (0..2).map(|_| tempfile::tempdir().unwrap()).collect();
    let a = start_at(dirs[0].path(), "Anna", |_| {}).await;
    let b = start_at(dirs[1].path(), "Bo", |_| {}).await;
    let (ad, bd) = (a.device_id(), b.device_id());
    let id: FolderId = a.chat_new("Read only", false).await.unwrap().id.parse().unwrap();
    join(&b, &a, &id, Role::ReadOnly, at(&a)).await;
    let mut png = Vec::new();
    image::DynamicImage::new_rgba8(4, 4).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
    let pasted = || vec![NewAttachment::Image(NewImage { name: "Pasted Image.png".into(), bytes: png.clone() })];

    // Bo may only read: his pasted picture is refused, and the conversation says so.
    let e = b.chat_send_attachments(id, None, "", None, None, pasted()).await.unwrap_err().to_string();
    let shown = b.chat_preparing(id);
    assert_eq!(shown.len(), 1);
    assert_eq!((shown[0].names.clone(), shown[0].problem.as_deref()), (vec!["Pasted Image.png".to_string()], Some(e.as_str())));
    assert!(e.contains("Read only"), "{e}");
    assert!(b.chat_dismiss_preparing(id, shown[0].id));
    assert!(b.chat_preparing(id).is_empty());

    // A picture for a device no longer connected directly: shown with the reason, nothing kept to send later.
    until(30, "the keys", || a.test_direct_knows(&bd) && b.test_direct_knows(&ad)).await;
    a.chat_send_direct(&bd, seedwell_sync_net::chat::MessageKind::Text, "hi", None, None).await.unwrap();
    b.test_refuse_direct(ad);
    until(20, "apart", || !a.chat_directs()[0].online).await;
    let e = a.chat_send_attachments(Place::Direct(bd), None, "", None, None, pasted()).await.unwrap_err().to_string();
    let shown = a.chat_preparing(Place::Direct(bd));
    assert_eq!(shown.len(), 1);
    assert_eq!(shown[0].problem.as_deref(), Some(e.as_str()));
    assert!(e.contains("directly"), "{e}");
    // A send that goes leaves nothing behind.
    let ok = a.chat_send_attachments(id, None, "", None, None, pasted()).await.unwrap();
    assert_eq!(ok.attachments.len(), 1);
    assert!(a.chat_preparing(id).is_empty());
    a.shutdown().await;
    b.shutdown().await;
}
