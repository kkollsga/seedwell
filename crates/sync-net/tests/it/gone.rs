//! Devices that are away or gone for good (SPEC §7G.5): a folder or channel whose other members are all away says
//! whom it waits for and since when; an owner removes a device that is gone; a member whose every owner and admin is
//! gone shares the folder again from its own device, its files untouched, and a new device joins that.

use std::path::Path;
use std::time::{Duration, SystemTime};

use trs_sync_net::status::FolderStatus;
use trs_sync_proto::ids::FolderId;
use trs_sync_proto::members::Role;

use crate::utp::{Device, addr, device_with, read, until};

async fn device(name: &str) -> Device {
    device_with(name, |_| {}).await
}

fn folder(d: &Device, id: &FolderId) -> Option<FolderStatus> {
    d.h.snapshot().folders.into_iter().find(|f| f.id == id.to_string())
}

/// Every file under `root` (not `.seedwell`), with its content, size and modification time.
fn files(root: &Path) -> Vec<(String, Vec<u8>, SystemTime)> {
    let mut out = Vec::new();
    let mut todo = vec![root.to_path_buf()];
    while let Some(dir) = todo.pop() {
        for e in std::fs::read_dir(&dir).unwrap() {
            let e = e.unwrap();
            let p = e.path();
            if p.file_name().is_some_and(|n| n == ".seedwell") {
                continue;
            }
            if e.file_type().unwrap().is_dir() {
                todo.push(p);
            } else {
                let rel = p.strip_prefix(root).unwrap().display().to_string();
                out.push((rel, std::fs::read(&p).unwrap(), e.metadata().unwrap().modified().unwrap()));
            }
        }
    }
    out.sort();
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_folder_and_a_channel_whose_other_member_stopped_say_whom_they_wait_for_and_the_owner_removes_it() {
    let dir = tempfile::tempdir().unwrap();
    let a = device("Anna").await;
    let docs = dir.path().join("Docs");
    std::fs::create_dir_all(&docs).unwrap();
    let id: FolderId = a.h.add_folder(docs).await.unwrap().id.parse().unwrap();
    assert_eq!(folder(&a, &id).unwrap().waiting, None, "nobody else in it: nobody to wait for");
    let ch: FolderId = a.h.chat_new("Plans", false).await.unwrap().id.parse().unwrap();
    let b = device("Bo").await;
    b.h.join_folder(&a.h.create_invite(&id, Role::ReadWrite, 1, 7).unwrap().link, dir.path().join("B")).await.unwrap();
    b.h.chat_join(&a.h.create_invite(&ch, Role::ReadWrite, 1, 7).unwrap().link).await.unwrap();
    until(30, "Bo is in both", || folder(&a, &id).is_some_and(|f| f.connected == 1) && a.h.channel_status(&ch).is_some_and(|c| c.online == 2)).await;
    let f = folder(&a, &id).unwrap();
    assert_eq!((f.waiting, f.gone, f.reshare), (None, vec![], false), "connected: nothing to say");
    let at = addr(&b);
    b.h.shutdown().await;

    let expect = format!("nothing answers at {at})");
    until(30, "Anna's folder waits for Bo, says since when and why", || {
        folder(&a, &id).and_then(|f| f.waiting).is_some_and(|w| w.starts_with("Waiting for Bo (last seen ") && w.ends_with(&expect))
    })
    .await;
    until(30, "and so does the channel", || a.h.channel_status(&ch).and_then(|c| c.waiting).is_some_and(|w| w.starts_with("Waiting for Bo (last seen "))).await;
    assert!(folder(&a, &id).unwrap().gone.is_empty(), "away a moment, not gone");

    a.h.test_gone_after(Duration::ZERO);
    let gone = folder(&a, &id).unwrap().gone;
    assert_eq!(gone.len(), 1, "{gone:?}");
    assert_eq!((gone[0].id.as_str(), gone[0].name.as_str()), (b.h.device_id().to_string().as_str(), "Bo"));
    assert!(gone[0].last_seen_s.is_some());
    assert_eq!(a.h.channel_status(&ch).unwrap().gone.len(), 1);
    a.h.remove_device(&id, &b.h.device_id()).unwrap();
    a.h.remove_device(&ch, &b.h.device_id()).unwrap();
    until(10, "Bo is removed from both: nobody to wait for", || {
        folder(&a, &id).is_some_and(|f| f.waiting.is_none() && f.gone.is_empty()) && a.h.channel_status(&ch).is_some_and(|c| c.waiting.is_none() && c.gone.is_empty())
    })
    .await;
    a.h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_member_whose_owner_is_gone_shares_the_folder_again_with_its_files_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let a = device("Anna").await;
    let docs = dir.path().join("Docs");
    std::fs::create_dir_all(docs.join("sub")).unwrap();
    std::fs::write(docs.join("seed.txt"), b"from Anna").unwrap();
    std::fs::write(docs.join("sub/deep.bin"), vec![7u8; 300_000]).unwrap();
    let id: FolderId = a.h.add_folder(docs).await.unwrap().id.parse().unwrap();
    let b = device("Bo").await;
    let place = dir.path().join("B");
    b.h.join_folder(&a.h.create_invite(&id, Role::ReadWrite, 1, 7).unwrap().link, place.clone()).await.unwrap();
    until(30, "Bo has Anna's files", || read(&place.join("sub/deep.bin")).is_some_and(|b| b.len() == 300_000) && folder(&b, &id).is_some_and(|f| f.need_files == 0)).await;
    std::fs::write(place.join("mine.txt"), b"Bo's own").unwrap();
    until(30, "Anna has Bo's file", || read(&dir.path().join("Docs/mine.txt")).is_some()).await;

    let err = b.h.reshare(&id).await.unwrap_err().to_string();
    assert!(err.contains("Anna, an owner of this folder, is connected"), "{err}");
    let err = a.h.reshare(&id).await.unwrap_err().to_string();
    assert!(err.contains("can share the folder itself"), "{err}");
    a.h.shutdown().await;

    until(30, "Bo waits for Anna", || folder(&b, &id).and_then(|f| f.waiting).is_some_and(|w| w.starts_with("Waiting for Anna (last seen "))).await;
    assert!(!folder(&b, &id).unwrap().reshare, "Anna is away a moment, not gone");
    b.h.test_gone_after(Duration::ZERO);
    let f = folder(&b, &id).unwrap();
    assert!(f.reshare, "every owner and admin is gone");
    assert!(f.gone.is_empty(), "a member removes nobody");
    let before = files(&place);
    assert_eq!(before.len(), 3, "{before:?}");

    let st = b.h.reshare(&id).await.unwrap();
    let new: FolderId = st.id.parse().unwrap();
    assert_ne!(new, id, "a new folder");
    assert_eq!((st.role, st.path.as_str()), (Some(Role::Owner), place.to_str().unwrap()), "owned by Bo, at the same place");
    assert!(folder(&b, &id).is_none() && b.h.configured(&id).is_none(), "the old folder is forgotten here");
    assert!(b.h.members(&id).is_err());
    let members = b.h.members(&new).unwrap();
    assert_eq!(members.len(), 1, "only Bo: the old folder's devices stay with it");
    until(30, "the new folder has read its files in", || folder(&b, &new).is_some_and(|f| f.state == trs_sync_net::status::FolderState::UpToDate && f.files == 3)).await;
    let f = folder(&b, &new).unwrap();
    assert_eq!((f.waiting, f.reshare, f.need_files), (None, false, 0), "nobody else in it yet, nothing to fetch");
    assert_eq!(files(&place), before, "no file was touched");

    let c = device("Cy").await;
    let to = dir.path().join("C");
    c.h.join_folder(&b.h.create_invite(&new, Role::ReadWrite, 1, 7).unwrap().link, to.clone()).await.unwrap();
    until(30, "Cy gets every file from Bo", || {
        read(&to.join("seed.txt")).as_deref() == Some(b"from Anna") && read(&to.join("mine.txt")).as_deref() == Some(b"Bo's own") && read(&to.join("sub/deep.bin")).is_some_and(|b| b.len() == 300_000)
    })
    .await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(files(&place), before, "still untouched, no conflict copy");
    assert_eq!(folder(&b, &new).unwrap().conflicts, 0);
    b.h.shutdown().await;
    c.h.shutdown().await;
}
