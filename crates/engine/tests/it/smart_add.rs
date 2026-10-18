//! Smart add (SPEC 7A.6): when a torrent is held for a choice, what a held torrent does and does
//! not do, and how the choice is answered.
use crate::common;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use common::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use trs_engine::config::{AfterAdd, AskMode, DetectionConfig, SmartAddConfig, WatchConfig, WatchMode};
use trs_engine::*;

struct Lab {
    dl: PathBuf,
    inc: PathBuf,
    state: PathBuf,
    _dirs: Vec<tempfile::TempDir>,
}

fn lab() -> Lab {
    let d: Vec<tempfile::TempDir> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    Lab { dl: d[0].path().into(), inc: d[1].path().join(".incomplete"), state: d[2].path().into(), _dirs: d }
}

/// Detection on, smart add on (`mode`), staging in the lab's `.incomplete`.
fn smart(l: &Lab, mode: AskMode, large_bytes: u64) -> impl FnOnce(&mut EngineConfig) + use<> {
    let inc = l.inc.clone();
    move |c| {
        c.detect = Some(DetectionConfig::default());
        c.incomplete_dir = Some(inc);
        c.use_staging = true;
        c.smart_add = SmartAddConfig { mode, large_bytes, ..SmartAddConfig::default() };
    }
}

const EP: usize = 200_000;

/// Two seasons of three episodes, a sample and an nfo.
fn tv_pack(announce: Option<String>) -> TestTorrent {
    let names: Vec<String> = (1..=2)
        .flat_map(|s| (1..=3).map(move |e| format!("Some.Show.S{s:02}E{e:02}.1080p.WEB-DL.x264-GRP.mkv")))
        .collect();
    let mut files: Vec<(&str, usize)> = names.iter().map(|n| (n.as_str(), EP)).collect();
    files.push(("Sample/sample.mkv", 30_000));
    files.push(("Some.Show.S01-S02.1080p.WEB-DL.x264-GRP.nfo", 2_000));
    make_torrent(&TorrentSpec {
        name: "Some.Show.S01-S02.1080p.WEB-DL.x264-GRP",
        piece_len: 32 * 1024,
        files: &files,
        private: false,
        announce,
        seed: 801,
    })
}

fn movie(seed: u64) -> TestTorrent {
    let n = "Night.of.the.Living.Dead.1968.1080p.BluRay.x264-GRP";
    make_torrent(&TorrentSpec {
        name: n,
        piece_len: 32 * 1024,
        files: &[
            ("Night.of.the.Living.Dead.1968.1080p.BluRay.x264-GRP.mkv", 600_000),
            ("Sample/sample.mkv", 50_000),
            ("Night.of.the.Living.Dead.1968.1080p.BluRay.x264-GRP.nfo", 2_000),
        ],
        private: false,
        announce: None,
        seed,
    })
}

async fn seeder_of(t: &TestTorrent) -> Node {
    let n = node(|_| {}).await;
    t.write_to(n.dir.path());
    n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&n, &t.id, 30).await;
    n
}

/// A TCP listener that only counts connections.
async fn counting_listener() -> (SocketAddr, Arc<AtomicU64>) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let n = Arc::new(AtomicU64::new(0));
    let n2 = n.clone();
    tokio::spawn(async move {
        loop {
            let Ok((s, _)) = l.accept().await else { break };
            n2.fetch_add(1, Ordering::Relaxed);
            drop(s);
        }
    });
    (addr, n)
}

use std::net::SocketAddr;

fn files_of_group(c: &ChoicePreview, kind: ChoiceGroupKind) -> Vec<u32> {
    c.groups.iter().filter(|g| g.kind == kind).flat_map(|g| g.items.iter().flat_map(|i| i.files.iter().copied())).collect()
}

fn only(files: &[u32], total: usize) -> Vec<FilePriority> {
    (0..total as u32).map(|i| if files.contains(&i) { FilePriority::Normal } else { FilePriority::Skip }).collect()
}

fn dir_is_empty(p: &Path) -> bool {
    std::fs::read_dir(p).map_or(true, |mut d| d.next().is_none())
}

fn write_file(p: &Path, data: &[u8]) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, data).unwrap();
}

fn reasons_has(c: &ChoicePreview, f: impl Fn(&ChoiceReason) -> bool) -> bool {
    c.reasons.iter().any(f)
}

/// A season pack needs a choice. While it waits it is invisible to the network and the disk:
/// no tracker request, no outgoing connection, incoming peers are turned away, nothing created.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_held_torrent_makes_no_requests_and_allocates_nothing() {
    let tracker = MockTracker::start(60).await;
    let t = tv_pack(Some(tracker.url()));
    let l = lab();
    let n = snode(&l.state, &l.dl, None, {
        let f = smart(&l, AskMode::Smart, 0);
        move |c| {
            f(c);
            c.trackers_enabled = true;
        }
    })
    .await;
    let (probe, conns) = counting_listener().await;
    let mut ev = n.h.subscribe();
    let added = n.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![probe], ..Default::default() }).await.unwrap();
    assert!(added.needs_choice);
    let c = added.choice.expect("the add carries the structure");
    assert!(c.awaiting && c.needs_choice);
    assert!(reasons_has(&c, |r| matches!(r, ChoiceReason::SeveralItems { items: 6 })), "{:?}", c.reasons);
    assert_eq!(c.structure, "tv");
    let seasons: Vec<_> = c.groups.iter().map(|g| g.kind).collect();
    assert_eq!(seasons[..2], [ChoiceGroupKind::Season(1), ChoiceGroupKind::Season(2)]);
    assert!(c.groups[0].items.iter().all(|i| i.selected), "everything is preselected when the library has nothing");
    let extras = c.groups.iter().find(|g| g.kind == ChoiceGroupKind::Extras).expect("extras");
    assert!(extras.items.iter().all(|i| !i.selected), "samples and nfo start unticked");
    wait_event(&mut ev, 5, |e| matches!(e, EngineEvent::ChoiceNeeded { id, .. } if *id == t.id)).await;
    assert_eq!(n.row(&t.id).await.unwrap().state, TorrentState::AwaitingChoice);

    // Peers offered while held are not dialled either.
    n.h.add_peers(t.id, vec![probe]);
    // An incoming peer is turned away: no handshake comes back.
    let mut s = TcpStream::connect(n.h.listen_port_addr()).await.unwrap();
    let mut hs = Vec::new();
    hs.extend(b"\x13BitTorrent protocol");
    hs.extend([0u8; 8]);
    hs.extend(t.id);
    hs.extend(b"-PR0001-probeprobepr");
    s.write_all(&hs).await.unwrap();
    let mut buf = [0u8; 68];
    let got = tokio::time::timeout(Duration::from_secs(2), s.read(&mut buf)).await;
    assert!(matches!(got, Ok(Ok(0)) | Ok(Err(_))), "a held torrent must not answer a handshake: {got:?}");
    tokio::time::sleep(Duration::from_millis(2500)).await;

    assert!(tracker.log_for(&t.id).is_empty(), "no announce while held: {:?}", tracker.log());
    assert_eq!(conns.load(Ordering::Relaxed), 0, "no connection attempt while held");
    assert!(dir_is_empty(&l.dl), "nothing in the download folder");
    assert!(!l.inc.exists(), "no staging folder");
    let snap = n.h.snapshot().await.unwrap();
    assert_eq!(snap.counts.downloading, 0);
    assert_eq!(snap.counts.queued, 0);
    assert_eq!(snap.rows[0].peers, 0);
}

trait PortAddr {
    fn listen_port_addr(&self) -> SocketAddr;
}
impl PortAddr for EngineHandle {
    fn listen_port_addr(&self) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], self.listen_port()))
    }
}

/// Confirming starts the torrent and downloads only the chosen files, into the torrent's own folder.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn confirm_starts_and_downloads_only_the_chosen_files() {
    let tracker = MockTracker::start(60).await;
    let t = tv_pack(Some(tracker.url()));
    let l = lab();
    let seeder = seeder_of(&t).await;
    let n = snode(&l.state, &l.dl, None, {
        let f = smart(&l, AskMode::Smart, 0);
        move |c| {
            f(c);
            c.trackers_enabled = true;
        }
    })
    .await;
    let added = n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    let c = added.choice.unwrap();
    let season2 = files_of_group(&c, ChoiceGroupKind::Season(2));
    assert_eq!(season2.len(), 3);
    n.h.confirm_choice(t.id, ChoiceSelection::Files(only(&season2, t.files.len()))).await.unwrap();
    n.h.add_peers(t.id, vec![seeder.addr()]);
    wait_done_h(&n.h, &t.id, 60).await;
    let row = n.row(&t.id).await.unwrap();
    assert_eq!(row.wanted_bytes, 3 * EP as u64);
    assert!(row.downloaded_total < 4 * EP as u64, "only the chosen season was downloaded: {}", row.downloaded_total);
    wait_for("placed", 20, || async { t.file_path(&l.dl, 5).exists().then_some(()) }).await;
    for e in 1..=3 {
        let p = t.file_path(&l.dl, 2 + e);
        assert!(std::fs::read(&p).unwrap() == t.files[2 + e].1, "{}", p.display());
    }
    // Season 1 was not chosen: none of it was downloaded (at most the edge of a shared piece).
    for i in 0..3 {
        if let Ok(got) = std::fs::read(t.file_path(&l.dl, i)) {
            let same = got.iter().zip(&t.files[i].1).filter(|(a, b)| a == b).count();
            assert!(same < EP / 2, "episode {i} of season 1 was downloaded");
        }
    }
    // The tracker heard from it only after the choice.
    wait_for("the first announce", 30, || async {
        tracker.log_for(&t.id).iter().any(|a| a.event.as_deref() == Some("started")).then_some(())
    })
    .await;
    // And the choice is gone from the state.
    assert_ne!(row.state, TorrentState::AwaitingChoice);
}

/// "You have some": items whose place in the torrent's own folder holds the same file, or the same
/// episode at another quality, are found, left unticked and labelled; the default choice
/// downloads the rest.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn items_already_in_the_library_are_found_and_left_unticked() {
    let t = tv_pack(None);
    let l = lab();
    let own = t.file_path(&l.dl, 0).parent().unwrap().to_path_buf();
    // E01 and E02: the very files; E03: the same episode at another size and name.
    write_file(&t.file_path(&l.dl, 0), &t.files[0].1);
    write_file(&t.file_path(&l.dl, 1), &t.files[1].1);
    write_file(&own.join("Some.Show.S01E03.720p.WEB-DL.x264-GRP.mkv"), &vec![7u8; 50_000]);
    let seeder = seeder_of(&t).await;
    let n = snode(&l.state, &l.dl, None, smart(&l, AskMode::Smart, 0)).await;
    let added = n.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();
    let c = added.choice.unwrap();
    assert!(c.library_checked);
    assert!(reasons_has(&c, |r| matches!(r, ChoiceReason::InLibrary { items: 3 })), "{:?}", c.reasons);
    let s1 = &c.groups[0];
    assert_eq!(s1.kind, ChoiceGroupKind::Season(1));
    assert!(matches!(s1.items[0].library, LibraryState::InLibrary { .. }));
    assert!(matches!(s1.items[1].library, LibraryState::InLibrary { .. }));
    assert!(matches!(s1.items[2].library, LibraryState::OtherQuality { .. }), "{:?}", s1.items[2].library);
    assert!(s1.items.iter().all(|i| !i.selected), "what the library has starts unticked");
    assert!(c.groups[1].items.iter().all(|i| i.selected && i.library == LibraryState::Missing));
    // Three episodes, plus the "other files" that are kept whenever something is chosen.
    assert_eq!(c.selected_bytes, 3 * EP as u64 + other_bytes(&c));
    // The default choice is exactly that.
    n.h.confirm_choice(t.id, ChoiceSelection::Default).await.unwrap();
    wait_done_h(&n.h, &t.id, 60).await;
    let row = n.row(&t.id).await.unwrap();
    assert_eq!(row.wanted_bytes, c.selected_bytes);
    wait_for("season 2 placed", 20, || async { t.file_path(&l.dl, 3).exists().then_some(()) }).await;
    // The user's own files were not touched.
    assert_eq!(std::fs::read(t.file_path(&l.dl, 0)).unwrap(), t.files[0].1);
    assert_eq!(std::fs::read(own.join("Some.Show.S01E03.720p.WEB-DL.x264-GRP.mkv")).unwrap(), vec![7u8; 50_000]);
}

fn other_bytes(c: &ChoicePreview) -> u64 {
    c.groups.iter().filter(|g| g.kind == ChoiceGroupKind::Other).flat_map(|g| &g.items).filter(|i| i.selected).map(|i| i.bytes).sum()
}

/// Software for several systems: this Mac's build is preselected.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn platform_variants_preselect_this_macs_build() {
    let t = make_torrent(&TorrentSpec {
        name: "CoolApp-2.1",
        piece_len: 32 * 1024,
        files: &[
            ("CoolApp-2.1-arm64.dmg", 120_000),
            ("CoolApp-2.1-x86_64.dmg", 130_000),
            ("CoolApp-2.1-win64.exe", 140_000),
            ("CoolApp-2.1-linux-x86_64.AppImage", 150_000),
        ],
        private: false,
        announce: None,
        seed: 802,
    });
    let l = lab();
    let n = snode(&l.state, &l.dl, None, smart(&l, AskMode::Smart, 0)).await;
    let p = n.h.preview(AddSource::TorrentFile(t.bytes.clone()), AddOptions::default()).await.unwrap().choice;
    assert!(reasons_has(&p, |r| matches!(r, ChoiceReason::PlatformVariants)), "{:?}", p.reasons);
    let picked: Vec<(&str, &str)> = p.groups.iter().flat_map(|g| &g.items).filter(|i| i.selected).filter_map(|i| i.platform).collect();
    assert_eq!(picked, [("macos", "arm64")], "{:?}", p.groups);
    assert!(p.groups.iter().flat_map(|g| &g.items).any(|i| !i.selected && i.platform.is_some_and(|p| p.0 == "windows")));
    assert_eq!(p.selected_bytes, 120_000);
    assert!(p.needs_choice);
}

/// The size triggers: over the configured limit, and over the free space.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn very_large_and_not_enough_space_hold_even_a_single_movie() {
    let t = movie(803);
    let l = lab();
    // 600 kB is "very large" against a 500 kB limit.
    let n = snode(&l.state, &l.dl, None, smart(&l, AskMode::Smart, 500_000)).await;
    let p = n.h.preview(AddSource::TorrentFile(t.bytes.clone()), AddOptions::default()).await.unwrap().choice;
    assert!(reasons_has(&p, |r| matches!(r, ChoiceReason::VeryLarge { limit: 500_000, .. })), "{:?}", p.reasons);
    assert!(p.needs_choice);
    drop(n);

    let l2 = lab();
    let n = snode(&l2.state, &l2.dl, None, {
        let f = smart(&l2, AskMode::Smart, 0);
        move |c| {
            f(c);
            c.smart_add.free_space_override = Some(100_000);
        }
    })
    .await;
    let added = n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    assert!(added.needs_choice);
    let c = added.choice.unwrap();
    assert!(reasons_has(&c, |r| matches!(r, ChoiceReason::NotEnoughSpace { free: 100_000, .. })), "{:?}", c.reasons);
    assert_eq!(c.free_bytes, Some(100_000));
}

/// A single movie with a confident match starts silently; an unsure one, or "ask always", is held.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn single_items_start_silently_unless_unsure_or_always() {
    let l = lab();
    let t = movie(804);
    let seeder = seeder_of(&t).await;
    let n = snode(&l.state, &l.dl, None, smart(&l, AskMode::Smart, 0)).await;
    let added = n.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();
    assert!(!added.needs_choice && added.choice.is_none(), "{:?}", added.choice.map(|c| c.reasons));
    wait_done_h(&n.h, &t.id, 60).await;

    // Unsure: a bare name with nothing to go on.
    let vague = make_torrent(&TorrentSpec {
        name: "blob",
        piece_len: 32 * 1024,
        files: &[("blob", 100_000)],
        private: false,
        announce: None,
        seed: 805,
    });
    let added = n.h.add_torrent_file(vague.bytes.clone(), AddOptions::default()).await.unwrap();
    assert!(added.needs_choice);
    assert!(reasons_has(&added.choice.unwrap(), |r| matches!(r, ChoiceReason::Unsure { .. })));
    // The caller can say "I have chosen" and skip the question.
    let added = n.h.add_torrent_file(vague.bytes.clone(), AddOptions { hold: Some(false), ..Default::default() }).await;
    assert!(matches!(added, Err(EngineError::Duplicate(_))));

    let l2 = lab();
    let n2 = snode(&l2.state, &l2.dl, None, smart(&l2, AskMode::Always, 0)).await;
    let added = n2.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    assert!(added.needs_choice);
    assert!(reasons_has(&added.choice.unwrap(), |r| matches!(r, ChoiceReason::Always)));

    // Smart add off: the same season pack just starts.
    let l3 = lab();
    let n3 = snode(&l3.state, &l3.dl, None, smart(&l3, AskMode::Never, 0)).await;
    let pack = tv_pack(None);
    let added = n3.h.add_torrent_file(pack.bytes.clone(), AddOptions { paused: true, ..Default::default() }).await.unwrap();
    assert!(!added.needs_choice);
}

/// A magnet fetches its metadata (that needs peers), then holds: no piece is requested.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_magnet_holds_after_its_metadata_and_requests_no_pieces() {
    let t = tv_pack(None);
    let l = lab();
    let seeder = seeder_of(&t).await;
    let n = snode(&l.state, &l.dl, None, smart(&l, AskMode::Smart, 0)).await;
    let mut ev = n.h.subscribe();
    let uri = format!("magnet:?xt=urn:btih:{}&dn={}", hex_of(&t.id), t.name);
    let added = n.h.add_magnet(&uri, AddOptions { peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();
    assert!(!added.needs_choice, "a magnet cannot know before its metadata");
    let e = wait_event(&mut ev, 30, |e| matches!(e, EngineEvent::ChoiceNeeded { id, .. } if *id == t.id)).await;
    let EngineEvent::ChoiceNeeded { reasons, .. } = e else { unreachable!() };
    assert!(reasons.iter().any(|r| matches!(r, ChoiceReason::SeveralItems { .. })), "{reasons:?}");
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let row = n.row(&t.id).await.unwrap();
    assert_eq!(row.state, TorrentState::AwaitingChoice);
    assert_eq!((row.done_bytes, row.downloaded_total, row.peers), (0, 0, 0), "{row:?}");
    let up = seeder.row(&t.id).await.unwrap().uploaded_total;
    assert_eq!(up, 0, "the seeder sent no payload");
    assert!(dir_is_empty(&l.dl), "nothing on disk");
    // The metadata is there, so the chooser works.
    let c = n.h.choice(t.id).await.unwrap();
    assert!(c.awaiting && !c.groups.is_empty());
    // Confirm: it starts and gets the chosen files.
    let s1 = files_of_group(&c, ChoiceGroupKind::Season(1));
    n.h.confirm_choice(t.id, ChoiceSelection::Files(only(&s1, t.files.len()))).await.unwrap();
    n.h.add_peers(t.id, vec![seeder.addr()]);
    wait_done_h(&n.h, &t.id, 60).await;
    assert_eq!(n.row(&t.id).await.unwrap().wanted_bytes, 3 * EP as u64);
    wait_for("placed", 20, || async { t.file_path(&l.dl, 2).exists().then_some(()) }).await;
    for i in 3..6 {
        if let Ok(got) = std::fs::read(t.file_path(&l.dl, i)) {
            let same = got.iter().zip(&t.files[i].1).filter(|(a, b)| a == b).count();
            assert!(same < EP / 2, "season 2 was not chosen but file {i} was downloaded");
        }
    }
}

/// Cancel removes the torrent and leaves nothing behind; the user's own files stay.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_leaves_nothing_and_never_touches_the_users_files() {
    let t = tv_pack(None);
    let l = lab();
    let own = t.file_path(&l.dl, 0);
    write_file(&own, &t.files[0].1);
    let n = snode(&l.state, &l.dl, None, smart(&l, AskMode::Smart, 0)).await;
    let mut ev = n.h.subscribe();
    let added = n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    assert!(added.needs_choice);
    n.h.cancel_choice(t.id).await.unwrap();
    wait_event(&mut ev, 5, |e| matches!(e, EngineEvent::Removed { id } if *id == t.id)).await;
    assert!(n.row(&t.id).await.is_none());
    assert_eq!(std::fs::read(&own).unwrap(), t.files[0].1, "the user's file is untouched");
    assert!(!l.inc.exists());
    // Only the user's file is in the download folder: no extra folder, nothing else.
    let mut found = Vec::new();
    fn walk(p: &Path, out: &mut Vec<PathBuf>) {
        for e in std::fs::read_dir(p).into_iter().flatten().flatten() {
            let q = e.path();
            if q.is_dir() {
                walk(&q, out);
            } else {
                out.push(q);
            }
        }
    }
    walk(&l.dl, &mut found);
    assert_eq!(found, std::slice::from_ref(&own));
    // The stored .torrent and the record are gone, and it can be added again.
    use trs_engine::persist::Store;
    n.h.flush().await.unwrap();
    assert!(n.store.load_torrents().unwrap().is_empty());
    let again = n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    assert!(again.needs_choice);
    // "Remove and delete files" on a held torrent deletes nothing: it never owned the user's files.
    n.h.remove(&[t.id], true).await.unwrap();
    assert_eq!(std::fs::read(&own).unwrap(), t.files[0].1, "the user's file survives");
    let again = n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    assert!(again.needs_choice);
    // A running torrent has nothing to cancel.
    let l2 = lab();
    let n2 = snode(&l2.state, &l2.dl, None, smart(&l2, AskMode::Never, 0)).await;
    let a = n2.h.add_torrent_file(t.bytes.clone(), AddOptions { paused: true, ..Default::default() }).await.unwrap();
    assert!(matches!(n2.h.cancel_choice(a.id).await, Err(EngineError::Invalid(_))));
    assert!(matches!(n2.h.cancel_choice([9; 20]).await, Err(EngineError::NotFound)));
    // Selecting nothing is refused.
    let r = n.h.confirm_choice(t.id, ChoiceSelection::Files(vec![FilePriority::Skip; t.files.len()])).await;
    assert!(matches!(r, Err(EngineError::Invalid(_))), "{r:?}");
}

/// Get Info > Files: items ticked later download into the same folder as the rest.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reopening_the_chooser_adds_items_into_the_same_folder() {
    let t = tv_pack(None);
    let l = lab();
    let seeder = seeder_of(&t).await;
    let n = snode(&l.state, &l.dl, None, smart(&l, AskMode::Smart, 0)).await;
    let added = n.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();
    let c = added.choice.unwrap();
    let s2 = files_of_group(&c, ChoiceGroupKind::Season(2));
    n.h.confirm_choice(t.id, ChoiceSelection::Files(only(&s2, t.files.len()))).await.unwrap();
    wait_done_h(&n.h, &t.id, 60).await;
    wait_for("placed", 20, || async { t.file_path(&l.dl, 5).exists().then_some(()) }).await;

    let r = n.h.reopen_choice(t.id).await.unwrap();
    assert!(!r.awaiting && !r.needs_choice);
    let ticked: Vec<bool> = r.groups.iter().filter(|g| matches!(g.kind, ChoiceGroupKind::Season(_))).map(|g| g.items.iter().all(|i| i.selected)).collect();
    assert_eq!(ticked, [false, true], "season 2 is ticked, season 1 is not");
    // Tick season 1 as well.
    let s1 = files_of_group(&r, ChoiceGroupKind::Season(1));
    let mut want = s2.clone();
    want.extend(&s1);
    n.h.confirm_choice(t.id, ChoiceSelection::Files(only(&want, t.files.len()))).await.unwrap();
    // A skipped file that the first download touched (the edge of a piece it shared with a
    // wanted file) exists as a mostly-empty file of full length, so "exists with the right
    // length" and even the row's byte counter (which also counts the unwanted rest of the
    // verified pieces) say nothing about season 1. Wait for the bytes themselves.
    wait_for("season 1 downloaded", 60, || async {
        let done = (0..3).all(|i| std::fs::read(t.file_path(&l.dl, i)).is_ok_and(|b| b == t.files[i].1));
        (done && n.row(&t.id).await.is_some_and(|r| r.wanted_bytes == 6 * EP as u64 && r.done_bytes >= 6 * EP as u64)).then_some(())
    })
    .await;
    for i in 0..6 {
        let p = t.file_path(&l.dl, i);
        assert!(std::fs::read(&p).unwrap() == t.files[i].1, "{}", p.display());
    }
}

/// A held torrent is still held after a restart, and still silent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_held_torrent_survives_a_restart_and_stays_silent() {
    let tracker = MockTracker::start(60).await;
    let t = tv_pack(Some(tracker.url()));
    let l = lab();
    let mk = |l: &Lab| {
        let f = smart(l, AskMode::Smart, 0);
        move |c: &mut EngineConfig| {
            f(c);
            c.trackers_enabled = true;
        }
    };
    let mut n = snode(&l.state, &l.dl, None, mk(&l)).await;
    n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    n.h.flush().await.unwrap();
    n.shutdown(Duration::from_secs(3)).await;
    drop(n);
    let n = snode(&l.state, &l.dl, None, mk(&l)).await;
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(n.row(&t.id).await.unwrap().state, TorrentState::AwaitingChoice);
    assert!(tracker.log_for(&t.id).is_empty(), "{:?}", tracker.log());
    assert!(dir_is_empty(&l.dl) && !l.inc.exists());
    let c = n.h.choice(t.id).await.unwrap();
    assert!(c.awaiting);
}

/// A watch folder that auto-adds a torrent needing a choice tells the app (dot + notification).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_watch_folder_add_that_needs_a_choice_emits_an_event() {
    let t = tv_pack(None);
    let l = lab();
    let watch = tempfile::tempdir().unwrap();
    let w = watch.path().to_path_buf();
    let n = snode(&l.state, &l.dl, None, {
        let f = smart(&l, AskMode::Smart, 0);
        move |c| {
            f(c);
            c.watch = vec![WatchConfig { path: w, mode: WatchMode::Auto, after_add: AfterAdd::Leave, category: None, rule: None, since: None }];
        }
    })
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut ev = n.h.subscribe();
    let file = watch.path().join("pack.torrent");
    std::fs::write(&file, &t.bytes).unwrap();
    let e = wait_event(&mut ev, 20, |e| matches!(e, EngineEvent::ChoiceNeeded { .. })).await;
    let EngineEvent::ChoiceNeeded { id, origin, reasons, .. } = e else { unreachable!() };
    assert_eq!(id, t.id);
    assert_eq!(origin, AddOrigin::Watch(file));
    assert!(reasons.iter().any(|r| matches!(r, ChoiceReason::SeveralItems { .. })));
    assert_eq!(n.row(&t.id).await.unwrap().state, TorrentState::AwaitingChoice);
}

/// A held torrent added from a `.torrent` file keeps that file's place for Cancel (SPEC 7A.6): only while it
/// waits, only when the file is a regular `.torrent` with the torrent's own info-hash.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_held_torrent_remembers_its_torrent_file_and_only_a_matching_one_counts() {
    let t = tv_pack(None);
    let other = movie(7);
    let l = lab();
    let n = snode(&l.state, &l.dl, None, smart(&l, AskMode::Smart, 0)).await;
    let file = l.state.join("pack.torrent");
    std::fs::write(&file, &t.bytes).unwrap();
    let added = n.h.add_torrent_path(&file, AddOptions::default(), Some(AfterAdd::Leave)).await.unwrap();
    assert!(added.needs_choice);
    assert!(n.h.has_held_source(t.id));
    assert_eq!(n.h.held_source(t.id), Some(file.clone()));

    // The file was swapped for another torrent's: no longer this torrent's file.
    std::fs::write(&file, &other.bytes).unwrap();
    assert_eq!(n.h.held_source(t.id), None, "a different info-hash is never offered");
    // Garbage and a wrong name are never offered either.
    std::fs::write(&file, b"not a torrent").unwrap();
    assert_eq!(n.h.held_source(t.id), None);
    std::fs::write(&file, &t.bytes).unwrap();
    assert_eq!(n.h.held_source(t.id), Some(file.clone()));
    // A link in its place is not a regular file.
    let real = l.state.join("real.dat");
    std::fs::rename(&file, &real).unwrap();
    std::os::unix::fs::symlink(&real, &file).unwrap();
    assert_eq!(n.h.held_source(t.id), None, "a symlink is never offered");
    std::fs::remove_file(&file).unwrap();
    assert!(!n.h.has_held_source(t.id), "a file that is gone is not offered");

    // Cancelling forgets it; the file (here: gone) is not the engine's business.
    n.h.cancel_choice(t.id).await.unwrap();
    assert!(!n.h.has_held_source(t.id));
}

/// The file's place follows "archive after adding", is dropped for "trash" and is forgotten once the torrent
/// is released.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_remembered_file_follows_the_after_add_action_and_goes_when_released() {
    let t = tv_pack(None);
    let l = lab();
    let (base, bin) = (smart(&l, AskMode::Smart, 0), l.state.join("bin"));
    // The trash folder is the lab's own: never the real one.
    let n = snode(&l.state, &l.dl, None, move |c| {
        base(c);
        c.trash_dir = Some(bin);
    })
    .await;
    let archive = l.state.join("archive");
    let file = l.state.join("pack.torrent");
    std::fs::write(&file, &t.bytes).unwrap();
    n.h.add_torrent_path(&file, AddOptions::default(), Some(AfterAdd::Archive(archive.clone()))).await.unwrap();
    assert_eq!(n.h.held_source(t.id), Some(archive.join("pack.torrent")), "the archived copy is the file that is left");
    n.h.confirm_choice(t.id, ChoiceSelection::Default).await.unwrap();
    assert!(!n.h.has_held_source(t.id), "released: nothing is remembered");

    let u = movie(8);
    let f2 = l.state.join("two.torrent");
    std::fs::write(&f2, &u.bytes).unwrap();
    n.h.add_torrent_path(&f2, AddOptions::default(), Some(AfterAdd::Trash)).await.unwrap();
    assert!(!n.h.has_held_source(u.id), "already in the trash folder: nothing left to offer");
}

/// `seedwell cancel` (the CLI's Cancel): the file is picked up before the cancel and put in the Trash folder after it,
/// and nothing but a regular `*.torrent` file is ever moved.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_helpers_move_the_held_torrent_file_to_the_trash_folder() {
    let t = tv_pack(None);
    let l = lab();
    let n = snode(&l.state, &l.dl, None, smart(&l, AskMode::Smart, 0)).await;
    let trash = l.state.join("trash");
    n.h.update_config({
        let trash = trash.clone();
        move |c| c.trash_dir = Some(trash)
    });
    let _ = n.h.config().await; // the change has been applied once this answers
    let file = l.state.join("pack.torrent");
    std::fs::write(&file, &t.bytes).unwrap();
    let added = n.h.add_torrent_path(&file, AddOptions::default(), Some(AfterAdd::Leave)).await.unwrap();
    assert!(added.needs_choice);
    let src = n.h.held_source_for_cancel(t.id).expect("a verified file");
    n.h.cancel_choice(t.id).await.unwrap();
    assert!(n.h.held_source_for_cancel(t.id).is_none(), "forgotten with the torrent");
    assert!(n.h.trash_file(&src));
    assert!(!file.exists(), "the file left its place");
    assert_eq!(std::fs::read_dir(&trash).unwrap().count(), 1, "and is in the Trash folder");
    let notes = l.state.join("notes.txt");
    std::fs::write(&notes, b"keep me").unwrap();
    assert!(!n.h.trash_file(&notes), "only a .torrent file is moved");
    assert!(notes.exists());
}
