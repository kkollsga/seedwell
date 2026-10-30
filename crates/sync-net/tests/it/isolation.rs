//! An isolated (test) run's Folder Sync (`Options::forbidden_root`): LAN only on a first start, joined folders in its
//! own state, and no folder inside the forbidden root (the real home), a temporary folder excepted.

use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use trs_sync_net::config::Settings;
use trs_sync_net::secrets::FileSecrets;
use trs_sync_net::{Options, SyncHandle};
use trs_sync_proto::ids::FolderId;
use trs_sync_proto::members::Role;

fn opts(state: &Path, forbidden_root: Option<PathBuf>) -> Options {
    let secrets = Arc::new(FileSecrets::new(state.join("secrets")));
    let mut o = Options::new(state.to_path_buf(), "Seedwell test".into(), secrets);
    o.bind = IpAddr::V4(Ipv4Addr::LOCALHOST);
    o.lan_discovery = false;
    o.forbidden_root = forbidden_root;
    o
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_isolated_run_syncs_on_the_lan_only_and_never_inside_the_real_home() {
    // A forbidden root outside the temporary folder, as a real home is (nothing is ever made there: every use of it
    // is refused).
    let root = PathBuf::from(if cfg!(windows) { r"C:\Users\seedwell-isolation-test" } else { "/Users/seedwell-isolation-test" });
    assert!(!root.exists());
    let state = tempfile::tempdir().unwrap();
    let h = SyncHandle::start(opts(state.path(), Some(root.clone()))).await.unwrap();
    let s = h.settings();
    assert!(s.lan_discovery, "the LAN is the one way an isolated run finds devices");
    assert!(!s.dht_discovery && !s.relay_own_devices && !s.relay_everyone, "nothing over the Internet: {s:?}");
    assert_eq!(Path::new(&s.default_location), state.path().join("Sync"), "joined folders go to the run's own state");

    // A plain run keeps the product's defaults.
    let plain_state = tempfile::tempdir().unwrap();
    let other = SyncHandle::start(opts(plain_state.path(), None)).await.unwrap();
    let ps = other.settings();
    assert!(ps.dht_discovery && ps.relay_own_devices && ps.default_location.is_empty(), "{ps:?}");

    // Inside the forbidden root: refused, and nothing is made there.
    let inside = root.join("Shared");
    let e = h.set_settings(Settings { default_location: inside.display().to_string(), ..h.settings() }).unwrap_err();
    assert!(e.to_string().contains("isolated"), "{e}");
    let shared = tempfile::tempdir().unwrap();
    let st = other.add_folder(shared.path().to_path_buf()).await.unwrap();
    let id: FolderId = st.id.parse().unwrap();
    let link = other.create_invite(&id, Role::ReadWrite, 1, 1).unwrap().link;
    let e = h.join_folder(&link, inside.clone()).await.unwrap_err();
    assert!(e.to_string().contains("isolated"), "{e}");
    assert!(!root.exists(), "nothing was made in the forbidden root");

    // A temporary folder is fine (Windows keeps it in the profile the run forbids).
    let tmp = tempfile::tempdir().unwrap();
    let joined = h.join_folder(&link, tmp.path().join("Shared")).await.unwrap();
    let jid: FolderId = joined.id.parse().unwrap();
    let e = h.move_folder(&jid, root.join("Moved")).await.unwrap_err();
    assert!(e.to_string().contains("isolated"), "{e}");

    // What the run chose itself is kept at its next start; only the default place is never the real home.
    h.set_settings(Settings { dht_discovery: true, ..h.settings() }).unwrap();
    h.shutdown().await;
    let h = SyncHandle::start(opts(state.path(), Some(root.clone()))).await.unwrap();
    let s = h.settings();
    assert!(s.dht_discovery && !s.relay_own_devices, "{s:?}");
    assert_eq!(Path::new(&s.default_location), state.path().join("Sync"));
    h.shutdown().await;
    other.shutdown().await;
    assert!(!root.exists());
}
