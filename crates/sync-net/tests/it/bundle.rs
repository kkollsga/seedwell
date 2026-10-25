//! One link for a folder and its channel (SPEC §7G.7): it admits to both at its role, a used-up or expired one
//! refuses both, revoking it revokes both, a device already in the channel only joins the folder, and a channel the
//! inviter may not share is refused when the link is made.

use trs_sync_net::status::FolderState;
use trs_sync_proto::ids::FolderId;
use trs_sync_proto::keys::FolderSecret;
use trs_sync_proto::members::{InviteLink, Role, Signer, new_invite};

use crate::chat::join as join_channel;
use crate::utp::{Device, addr, device_with, until};

async fn device(name: &str) -> Device {
    device_with(name, |_| {}).await
}

/// An owner with a folder `Docs` and a channel `Docs chat`.
async fn owner(dir: &std::path::Path) -> (Device, FolderId, FolderId) {
    let a = device("Anna").await;
    let docs = dir.join("Docs");
    std::fs::create_dir_all(&docs).unwrap();
    let folder: FolderId = a.h.add_folder(docs).await.unwrap().id.parse().unwrap();
    let channel: FolderId = a.h.chat_new("Docs chat").await.unwrap().id.parse().unwrap();
    (a, folder, channel)
}

fn folder_of(d: &Device, id: &FolderId) -> Option<trs_sync_net::status::FolderStatus> {
    d.h.snapshot().folders.into_iter().find(|f| f.id == id.to_string())
}

/// `d` joins with `link` at `dir/<name>` and dials `via` for the folder only: the channel comes over the same
/// session.
async fn join_both(d: &Device, link: &str, dir: &std::path::Path, name: &str, folder: &FolderId, channel: &FolderId, via: &Device) {
    let f = d.h.join_folder(link, dir.join(name)).await.unwrap();
    assert_eq!(f.channel_id, Some(channel.to_string()), "linked on the joining device");
    d.h.add_address(folder, addr(via)).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_link_for_both_admits_to_both_and_once_used_up_refuses_both() {
    let dir = tempfile::tempdir().unwrap();
    let (a, folder, channel) = owner(dir.path()).await;
    let inv = a.h.create_bundle_invite(&folder, Role::ReadOnly, 1, 7, &channel).unwrap();
    assert_eq!((inv.channel_id.clone(), inv.channel_name.as_deref()), (Some(channel.to_string()), Some("Docs chat")));
    assert!(inv.link.starts_with("seedwell://sync/") && inv.link.contains("&chat="), "{}", inv.link);
    assert_eq!(folder_of(&a, &folder).unwrap().channel_id, Some(channel.to_string()), "the owner's folder goes with it too");
    let ch_invites = a.h.invites(&channel).unwrap();
    assert_eq!((ch_invites.len(), ch_invites[0].role, ch_invites[0].max_uses, ch_invites[0].expires_s), (1, Role::ReadOnly, 1, inv.expires_s), "the channel lists its half");

    let b = device("Bo").await;
    join_both(&b, &inv.link, dir.path(), "B", &folder, &channel, &a).await;
    until(30, "B is in both, read only", || {
        folder_of(&b, &folder).is_some_and(|f| f.role == Some(Role::ReadOnly)) && b.h.channel_status(&channel).is_some_and(|c| c.role == Some(Role::ReadOnly))
    })
    .await;
    assert_eq!(b.h.channel_status(&channel).unwrap().name, "Docs chat");

    // Used up: the next device is refused for both.
    let c = device("Cy").await;
    join_both(&c, &inv.link, dir.path(), "C", &folder, &channel, &a).await;
    until(30, "C is refused for both", || {
        let f = folder_of(&c, &folder).is_some_and(|f| f.state == FolderState::Error && f.error.as_deref().is_some_and(|e| e.contains("used up")));
        let ch = c.h.channel_status(&channel).is_some_and(|ch| ch.role.is_none() && ch.error.as_deref().is_some_and(|e| e.contains("used up")));
        f && ch
    })
    .await;
    for d in [&a, &b, &c] {
        d.h.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_expired_channel_half_refuses_the_whole_join() {
    let dir = tempfile::tempdir().unwrap();
    let (a, folder, _) = owner(dir.path()).await;
    let good = InviteLink::parse(&a.h.create_invite(&folder, Role::ReadWrite, 1, 7).unwrap().link).unwrap();
    // A channel invitation that expired yesterday.
    let s = FolderSecret::from_bytes([4; 32]);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let old = new_invite(*s.public_key(), [5; 16], [6; 32], Role::ReadWrite, 1, now - 86_400, now - 8 * 86_400, Signer::Folder, &|m: &[u8]| s.sign(m));
    let link = good.format_bundle(&InviteLink { invite: old, token_seed: [6; 32], name: Some("Old".into()) });
    let b = device("Bo").await;
    let e = b.h.join_folder(&link, dir.path().join("B")).await.unwrap_err().to_string();
    assert!(e.contains("expired"), "{e}");
    assert!(b.h.snapshot().folders.is_empty() && b.h.chat_channels().is_empty(), "nothing joined");
    a.h.shutdown().await;
    b.h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revoking_a_link_for_both_revokes_both_and_a_member_of_the_channel_only_joins_the_folder() {
    let dir = tempfile::tempdir().unwrap();
    let (a, folder, channel) = owner(dir.path()).await;
    let first = a.h.create_bundle_invite(&folder, Role::ReadWrite, 1, 7, &channel).unwrap();
    a.h.revoke_invite(&folder, &first.id).unwrap();
    assert!(a.h.invites(&folder).unwrap().iter().all(|i| i.revoked));
    assert!(a.h.invites(&channel).unwrap().iter().all(|i| i.revoked), "the channel's half too");

    // B is in the channel already: the link for both joins only the folder.
    let b = device("Bo").await;
    join_channel(&b.h, &a.h, &channel, Role::ReadWrite, addr(&a)).await;
    let inv = a.h.create_bundle_invite(&folder, Role::ReadWrite, 1, 7, &channel).unwrap();
    join_both(&b, &inv.link, dir.path(), "B", &folder, &channel, &a).await;
    until(30, "B is in the folder", || folder_of(&b, &folder).is_some_and(|f| f.role == Some(Role::ReadWrite))).await;
    assert_eq!(b.h.chat_channels().len(), 1);
    let half = trs_sync_proto::members::hex(&InviteLink::parse_bundle(&inv.link).unwrap().1.unwrap().invite.id);
    let ch_invites = a.h.invites(&channel).unwrap();
    assert!(ch_invites.iter().any(|i| i.id == half && !i.revoked && i.uses == 0), "the channel's half of the new link is not used: {ch_invites:?}");
    assert!(b.h.set_folder_channel(&folder, Some(&FolderId::of_public_key(&[9; 32]))).is_err(), "not a channel here");
    b.h.set_folder_channel(&folder, None).unwrap();
    assert_eq!(folder_of(&b, &folder).unwrap().channel_id, None);
    a.h.shutdown().await;
    b.h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_channel_the_inviter_may_not_share_is_refused_when_the_link_is_made() {
    let dir = tempfile::tempdir().unwrap();
    let (a, _, channel) = owner(dir.path()).await;
    let b = device("Bo").await;
    join_channel(&b.h, &a.h, &channel, Role::ReadWrite, addr(&a)).await;
    let mine = dir.path().join("Mine");
    std::fs::create_dir_all(&mine).unwrap();
    let folder: FolderId = b.h.add_folder(mine).await.unwrap().id.parse().unwrap();
    let e = b.h.create_bundle_invite(&folder, Role::ReadWrite, 1, 7, &channel).unwrap_err().to_string();
    assert!(e.contains("may not share the channel Docs chat"), "{e}");
    assert!(b.h.invites(&folder).unwrap().is_empty() && a.h.invites(&channel).unwrap().len() == 1, "nothing made");
    a.h.shutdown().await;
    b.h.shutdown().await;
}
