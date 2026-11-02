//! Ignored paths between two services on loopback (temp folders only, SPEC §7G.2): the case found by syncing a
//! folder of Claude skills between a Mac and a PC. The PC joined, got the folder's `.seedwellignore` (`/synced/`), and
//! Claude Code then wrote `synced/` into the folder through a junction: the PC indexed and announced all of it, and
//! the Mac, which ignores `synced/` too, counted it as to fetch and waited for it forever.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use trs_sync_net::config::SyncConfig;
use trs_sync_net::secrets::FileSecrets;
use trs_sync_net::status::FolderState;
use trs_sync_net::{Options, SyncHandle};
use trs_sync_proto::ids::FolderId;
use trs_sync_proto::members::Role;

const RULES: &[u8] = b"// Claude Code's own state\n/synced/\n/_snapshots/\n.venv/\nvenv/\n__pycache__/\n*.pyc\nnode_modules/\n.DS_Store\nThumbs.db\n";

async fn device_in(name: &str, state: &Path) -> SyncHandle {
    let secrets = Arc::new(FileSecrets::new(state.join("secrets")));
    let mut opts = Options::new(state.to_path_buf(), "Seedwell test".into(), secrets);
    opts.default_device_name = name.into();
    opts.bind = IpAddr::V4(Ipv4Addr::LOCALHOST);
    opts.lan_discovery = false;
    SyncHandle::start(opts).await.unwrap()
}

fn addr(h: &SyncHandle) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), h.port())
}

/// Waits (polling) until `ok` holds, or fails after `secs`.
async fn until(secs: u64, what: &str, mut ok: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for: {what}");
}

/// For `secs`, `ok` holds every time it is looked at.
async fn stays(secs: u64, what: &str, mut ok: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        assert!(ok(), "no longer true: {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn read(p: &Path) -> Option<Vec<u8>> {
    std::fs::read(p).ok()
}

/// A link at `link` to the folder `target`: a symlink, or on Windows a directory junction (as the user's
/// `%USERPROFILE%\.claude\skills` was made to the synced folder).
fn make_dir_link(target: &Path, link: &Path) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
    #[cfg(windows)]
    {
        let ok = std::process::Command::new("cmd").arg("/C").arg("mklink").arg("/J").arg(link).arg(target).output().unwrap();
        assert!(ok.status.success(), "mklink /J: {ok:?}");
    }
}

fn up_to_date(h: &SyncHandle) -> bool {
    let f = &h.snapshot().folders[0];
    f.state == FolderState::UpToDate && f.need_files == 0
}

/// Nothing to fetch and not waiting for a device (a routine scan, or a pull round that finds nothing, may pass).
fn not_waiting(h: &SyncHandle) -> bool {
    let f = &h.snapshot().folders[0];
    f.need_files == 0 && f.state != FolderState::WaitingForDevices && f.state != FolderState::Error
}

/// The PC's case end to end: the rules arrive after the folder started on the joining device, then files they ignore
/// are written into it through a link. They are never indexed or announced there, so the owner never waits for them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn files_written_through_a_link_after_the_rules_arrived_are_never_announced() {
    let (sa, sb) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (mac, pc) = (device_in("Mac", sa.path()).await, device_in("PC", sb.path()).await);
    let (fa, fb) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    std::fs::write(fa.path().join(".seedwellignore"), RULES).unwrap();
    std::fs::write(fa.path().join("real.md"), b"a skill").unwrap();
    let st = mac.add_folder(fa.path().to_path_buf()).await.unwrap();
    let id: FolderId = st.id.parse().unwrap();
    let link = mac.create_invite(&id, Role::ReadWrite, 1, 1).unwrap().link;
    let dest = fb.path().join("Claude Skills");
    pc.join_folder(&link, dest.clone()).await.unwrap();
    pc.add_address(&id, addr(&mac)).unwrap();
    until(30, "the PC has the rules", || read(&dest.join(".seedwellignore")).as_deref() == Some(RULES)).await;
    until(30, "the PC has the skill", || read(&dest.join("real.md")).is_some()).await;

    // Claude Code writes its state through the link.
    let view = fb.path().join("skills");
    make_dir_link(&dest, &view);
    std::fs::create_dir_all(view.join("synced").join("u1_u2")).unwrap();
    std::fs::write(view.join("synced").join(".bucket-1"), b"bucket").unwrap();
    for i in 0..20 {
        std::fs::write(view.join("synced").join("u1_u2").join(format!("skill{i}.md")), format!("skill {i}")).unwrap();
    }
    std::fs::create_dir_all(view.join("tool").join("__pycache__")).unwrap();
    std::fs::write(view.join("tool").join("__pycache__").join("m.pyc"), b"bytecode").unwrap();
    // And a file that does sync, written after them: once it is on the Mac, the PC has scanned what came before.
    std::fs::write(view.join("tool").join("run.py"), b"print()").unwrap();
    until(30, "the Mac has the PC's new file", || read(&fa.path().join("tool").join("run.py")).is_some()).await;

    until(20, "both up to date", || up_to_date(&mac) && up_to_date(&pc)).await;
    stays(4, "nothing to fetch, nothing waited for", || not_waiting(&mac) && not_waiting(&pc)).await;
    until(20, "both up to date again", || up_to_date(&mac) && up_to_date(&pc)).await;
    assert!(!fa.path().join("synced").exists(), "nothing of synced/ reached the Mac");
    assert!(!fa.path().join("tool").join("__pycache__").exists());
    // The PC counts the files it syncs: .seedwellignore, real.md, tool/run.py.
    assert_eq!(pc.snapshot().folders[0].files, 3, "{:?}", pc.snapshot().folders[0]);
    assert!(dest.join("synced").join("u1_u2").join("skill0.md").exists(), "left alone on disk");
    mac.shutdown().await;
    pc.shutdown().await;
}

/// Device A indexes a path that B ignores (B's own, local-only patterns): B ends up up to date with nothing of it
/// fetched or counted, A stays up to date too, and B's own file there is left alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_path_announced_by_a_device_that_does_not_ignore_it_is_never_fetched_here() {
    let (sa, sb) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = device_in("Alpha", sa.path()).await;
    let mut b = device_in("Beta", sb.path()).await;
    let (fa, fb) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    std::fs::write(fa.path().join("doc.md"), b"doc").unwrap();
    let st = a.add_folder(fa.path().to_path_buf()).await.unwrap();
    let id: FolderId = st.id.parse().unwrap();
    let link = a.create_invite(&id, Role::ReadWrite, 1, 1).unwrap().link;
    let dest = fb.path().join("Shared");
    b.join_folder(&link, dest.clone()).await.unwrap();
    b.add_address(&id, addr(&a)).unwrap();
    until(30, "B has the doc", || read(&dest.join("doc.md")).is_some()).await;

    // B's own patterns (set in its folder's settings; here in its config while it is stopped).
    b.shutdown().await;
    drop(b);
    let cfg_path = sb.path().join("sync.toml");
    let mut cfg = SyncConfig::load(&cfg_path).unwrap();
    cfg.folder_mut(&st.id).unwrap().ignore = vec!["cache/".into(), "*.tmp".into()];
    cfg.save(&cfg_path).unwrap();
    std::fs::create_dir(dest.join("cache")).unwrap();
    std::fs::write(dest.join("cache").join("big.bin"), b"B's own, ignored here").unwrap();
    b = device_in("Beta", sb.path()).await;
    b.add_address(&id, addr(&a)).unwrap();

    std::fs::create_dir_all(fa.path().join("cache").join("sub")).unwrap();
    std::fs::write(fa.path().join("cache").join("big.bin"), vec![7u8; 2_000_000]).unwrap();
    std::fs::write(fa.path().join("cache").join("sub").join("x.txt"), b"x").unwrap();
    std::fs::write(fa.path().join("scratch.tmp"), b"tmp").unwrap();
    std::fs::write(fa.path().join("later.md"), b"later").unwrap();
    until(30, "B has the later doc", || read(&dest.join("later.md")).is_some()).await;
    until(20, "both up to date", || up_to_date(&a) && up_to_date(&b)).await;
    stays(4, "nothing to fetch, nothing waited for", || not_waiting(&a) && not_waiting(&b)).await;
    until(20, "both up to date again", || up_to_date(&a) && up_to_date(&b)).await;
    assert_eq!(read(&dest.join("cache").join("big.bin")).as_deref(), Some(&b"B's own, ignored here"[..]), "left alone");
    assert!(!dest.join("cache").join("sub").exists(), "nothing fetched below cache/");
    assert!(!dest.join("scratch.tmp").exists());
    // B's own file was never announced: A keeps its own.
    assert_eq!(read(&fa.path().join("cache").join("big.bin")).map(|v| v.len()), Some(2_000_000));
    a.shutdown().await;
    b.shutdown().await;
}

/// A skill deleted on the Mac goes on the PC too, with the caches, virtual environment and OS files the PC made in it
/// (`(?d)` lines, built-in OS metadata). A folder that also holds ignored files without `(?d)` stays: the PC says so
/// once, as an issue, and is up to date meanwhile; once those files are gone, the folder goes too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_folder_deleted_on_one_device_goes_on_the_other_with_its_deletable_junk() {
    const DELETABLE: &[u8] = b"/synced/\n(?d)__pycache__/\n(?d)*.pyc\n(?d).venv/\nnode_modules/\n";
    let (sa, sb) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (mac, pc) = (device_in("Mac", sa.path()).await, device_in("PC", sb.path()).await);
    let (fa, fb) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    std::fs::write(fa.path().join(".seedwellignore"), DELETABLE).unwrap();
    for d in ["skill", "tool"] {
        std::fs::create_dir(fa.path().join(d)).unwrap();
        std::fs::write(fa.path().join(d).join("main.py"), b"print()").unwrap();
    }
    let st = mac.add_folder(fa.path().to_path_buf()).await.unwrap();
    let id: FolderId = st.id.parse().unwrap();
    let link = mac.create_invite(&id, Role::ReadWrite, 1, 1).unwrap().link;
    let dest = fb.path().join("Claude Skills");
    pc.join_folder(&link, dest.clone()).await.unwrap();
    pc.add_address(&id, addr(&mac)).unwrap();
    until(30, "the PC has both skills", || read(&dest.join("skill").join("main.py")).is_some() && read(&dest.join("tool").join("main.py")).is_some()).await;

    // What the PC makes in them, and never syncs.
    let s = dest.join("skill");
    std::fs::create_dir_all(s.join("__pycache__")).unwrap();
    std::fs::write(s.join("__pycache__").join("main.cpython-313.pyc"), b"bytecode").unwrap();
    std::fs::create_dir_all(s.join(".venv").join("Scripts")).unwrap();
    std::fs::write(s.join(".venv").join("Scripts").join("python.exe"), b"venv").unwrap();
    std::fs::write(s.join("Thumbs.db"), b"thumbs").unwrap();
    std::fs::write(s.join("desktop.ini"), b"[.ShellClassInfo]").unwrap();
    std::fs::write(s.join(".DS_Store"), b"finder").unwrap();
    let t = dest.join("tool");
    std::fs::create_dir_all(t.join("node_modules")).unwrap();
    std::fs::write(t.join("node_modules").join("m.js"), b"module").unwrap();
    std::fs::write(t.join(".DS_Store"), b"finder").unwrap();
    until(20, "both up to date", || up_to_date(&mac) && up_to_date(&pc)).await;
    assert_eq!(pc.snapshot().folders[0].files, 3, "{:?}", pc.snapshot().folders[0]);

    // Both are deleted on the Mac.
    std::fs::remove_dir_all(fa.path().join("skill")).unwrap();
    std::fs::remove_dir_all(fa.path().join("tool")).unwrap();
    until(30, "the skill is gone on the PC, junk and all", || !s.exists()).await;
    let kept = |pc: &SyncHandle| pc.snapshot().folders[0].issues.iter().any(|(p, why)| p == "tool" && why.contains("node_modules"));
    until(30, "the tool is kept, with an issue", || kept(&pc) && read(&t.join("node_modules").join("m.js")).is_some()).await;
    assert!(!t.join("main.py").exists(), "its synced file went");
    until(20, "both up to date", || up_to_date(&mac) && up_to_date(&pc)).await;
    stays(12, "the PC does not wait for the kept folder, nor try it again and again", || not_waiting(&pc) && kept(&pc)).await;

    // The modules go: so does the folder, and its issue.
    std::fs::remove_dir_all(t.join("node_modules")).unwrap();
    until(30, "the tool is gone on the PC", || !t.exists()).await;
    until(20, "the issue is gone and both are up to date", || !kept(&pc) && up_to_date(&mac) && up_to_date(&pc)).await;
    assert!(!fa.path().join("tool").exists() && !fa.path().join("skill").exists(), "nothing came back on the Mac");
    mac.shutdown().await;
    pc.shutdown().await;
}

/// A device's own patterns, changed while it runs, apply at once: what they ignore is no longer fetched, and once they
/// are gone again it arrives. They are kept in its settings.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn own_patterns_changed_while_running_apply_at_once() {
    let (sa, sb) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (a, b) = (device_in("Alpha", sa.path()).await, device_in("Beta", sb.path()).await);
    let (fa, fb) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    std::fs::write(fa.path().join("doc.md"), b"doc").unwrap();
    let st = a.add_folder(fa.path().to_path_buf()).await.unwrap();
    let id: FolderId = st.id.parse().unwrap();
    let link = a.create_invite(&id, Role::ReadWrite, 1, 1).unwrap().link;
    let dest = fb.path().join("Shared");
    b.join_folder(&link, dest.clone()).await.unwrap();
    b.add_address(&id, addr(&a)).unwrap();
    until(30, "B has the doc", || read(&dest.join("doc.md")).is_some()).await;

    b.set_ignore_patterns(&id, vec!["cache/".into()]).await.unwrap();
    let cfg = SyncConfig::load(&sb.path().join("sync.toml")).unwrap();
    assert_eq!(cfg.folders.iter().find(|e| e.id == st.id).unwrap().ignore, ["cache/"], "kept in its settings");
    std::fs::create_dir(fa.path().join("cache")).unwrap();
    std::fs::write(fa.path().join("cache").join("big.bin"), vec![1u8; 100_000]).unwrap();
    std::fs::write(fa.path().join("later.md"), b"later").unwrap();
    until(30, "B has the later doc", || read(&dest.join("later.md")).is_some()).await;
    until(20, "both up to date", || up_to_date(&a) && up_to_date(&b)).await;
    assert!(!dest.join("cache").exists(), "not fetched while ignored");

    b.set_ignore_patterns(&id, Vec::new()).await.unwrap();
    until(30, "B fetches it now", || read(&dest.join("cache").join("big.bin")).map(|v| v.len()) == Some(100_000)).await;
    until(20, "both up to date", || up_to_date(&a) && up_to_date(&b)).await;
    a.shutdown().await;
    b.shutdown().await;
}
