//! Lazy hashing of on-demand folders (SPEC §7G.11, *Listed first, hashed when first fetched*): an on-demand folder is
//! listed on every device within seconds of being added, whatever its size; a file's blocks are hashed only when a
//! device first fetches it, as part of that download. Temp folders and loopback only.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use trs_sync::ondemand::ROOT_ID;
use trs_sync_net::secrets::FileSecrets;
use trs_sync_net::{Options, SyncHandle};
use trs_sync_proto::ids::FolderId;
use trs_sync_proto::members::Role;

struct Device {
    h: SyncHandle,
    _state: tempfile::TempDir,
}

async fn device(name: &str) -> Device {
    let state = tempfile::tempdir().unwrap();
    let secrets = Arc::new(FileSecrets::new(state.path().join("secrets")));
    let mut opts = Options::new(state.path().to_path_buf(), "Seedwell test".into(), secrets);
    opts.default_device_name = name.into();
    opts.bind = IpAddr::V4(Ipv4Addr::LOCALHOST);
    opts.lan_discovery = false;
    Device { h: SyncHandle::start(opts).await.unwrap(), _state: state }
}

fn addr(d: &Device) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), d.h.port())
}

async fn until(secs: u64, what: &str, mut ok: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for: {what}");
}

/// How many items `d` lists in the folder, every level.
fn listed(d: &Device, id: &FolderId) -> usize {
    let mut n = 0;
    let mut dirs = vec![ROOT_ID.to_string()];
    while let Some(dir) = dirs.pop() {
        let mut cursor: Option<String> = None;
        while let Ok(Some(page)) = d.h.od_children(id, &dir, cursor.as_deref(), 1000) {
            for i in &page.items {
                n += 1;
                if i.kind == trs_sync_proto::model::FileKind::Dir {
                    dirs.push(i.id.clone());
                }
            }
            match page.next {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }
    }
    n
}

/// A folder of `films` files of `size` bytes each (sparse where the disk makes them so: APFS and ext4 do, NTFS does not,
/// so the big ones are not made on Windows; hashing them reads every byte all the same) and `extras` small files in a
/// subfolder.
fn movies(root: &std::path::Path, films: usize, size: u64, extras: usize) -> usize {
    use std::io::Write;
    std::fs::create_dir_all(root.join("Extras")).unwrap();
    for i in 0..films {
        let mut f = std::fs::File::create(root.join(format!("Film {i:03}.mkv"))).unwrap();
        // A first byte of its own, so no two films are the same content.
        f.write_all(&[i as u8 + 1]).unwrap();
        f.set_len(size).unwrap();
    }
    for i in 0..extras {
        std::fs::write(root.join("Extras").join(format!("note {i:03}.txt")), format!("note {i}")).unwrap();
    }
    films + extras + 1
}

/// The failure: an on-demand folder's add reads and hashes every byte before its files are announced, so the other
/// devices list nothing for as long as that takes (Seedwell 0.1.18: 28 minutes for 914 GB, the file managers' spinners
/// endless meanwhile). Added as the app adds it with *Files on demand* (a full folder here, its files kept in place, on
/// demand for the others). With 64 GiB of films the old way takes about half a minute on this Mac's SSD; listed first,
/// it takes about a second.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[cfg_attr(windows, ignore = "64 GiB of films: NTFS makes no sparse files")]
async fn a_big_on_demand_folder_is_listed_on_a_peer_within_seconds_of_being_added() {
    let (a, b) = (device("Alpha").await, device("Beta").await);
    let (fa, fb) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let src = fa.path().join("Movies");
    let items = movies(&src, 64, 1 << 30, 300);
    let started = Instant::now();
    let st = a.h.add_folder_with(src, true).await.unwrap();
    assert!(st.on_demand_others && !st.on_demand, "full here, on demand for the others");
    let id: FolderId = st.id.parse().unwrap();
    let link = a.h.create_invite(&id, Role::ReadWrite, 1, 1).unwrap().link;
    b.h.join_folder_on_demand(&link, fb.path().join("Movies")).await.unwrap();
    b.h.add_address(&id, addr(&a)).unwrap();
    until(20, "B lists every file of A's folder", || listed(&b, &id) == items).await;
    let took = started.elapsed();
    eprintln!("listed on the peer {took:?} after the add ({items} items, 64 GiB)");
    assert!(took < Duration::from_secs(10), "listed only after {took:?}");
    let st = a.h.snapshot().folders.into_iter().find(|f| f.id == id.to_string()).unwrap();
    assert!(st.files as usize >= items - 1, "the owner counts its files at once too: {}", st.files);
    for d in [&a, &b] {
        d.h.shutdown().await;
    }
}

/// Bytes that differ throughout (so every chunk is its own).
fn noise(n: usize, seed: u64) -> Vec<u8> {
    let mut s = seed;
    (0..n)
        .map(|_| {
            s = s.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            (s >> 33) as u8
        })
        .collect()
}

/// An owner with `files` (path, bytes), and an on-demand member that lists them. `freed`: the owner's copies are in its
/// cache (on demand there too); else the owner's folder is full, its files in place, on demand for the others (the
/// app's *Files on demand*).
async fn owner_and_member(files: &[(&str, Vec<u8>)], freed: bool) -> (Device, Device, tempfile::TempDir, tempfile::TempDir, FolderId) {
    let (a, b) = (device("Alpha").await, device("Beta").await);
    let (fa, fb) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let src = fa.path().join("Movies");
    for (p, bytes) in files {
        let at = src.join(p);
        std::fs::create_dir_all(at.parent().unwrap()).unwrap();
        std::fs::write(at, bytes).unwrap();
    }
    let st = if freed { a.h.add_folder_on_demand(src).await } else { a.h.add_folder_with(src, true).await };
    let id: FolderId = st.unwrap().id.parse().unwrap();
    let link = a.h.create_invite(&id, Role::ReadWrite, 5, 1).unwrap().link;
    b.h.join_folder_on_demand(&link, fb.path().join("Movies")).await.unwrap();
    b.h.add_address(&id, addr(&a)).unwrap();
    until(30, "B lists A's files", || files.iter().all(|(p, _)| b.h.od_item_at(&id, p).ok().flatten().is_some())).await;
    (a, b, fa, fb, id)
}

/// The failures: a device that opens a file its maker has not hashed yet gets nothing (there is no chunk list to ask
/// by) or bytes nobody checked; and once one device has it, another cannot fetch it from that copy (no hashes recorded
/// anywhere), so every later download needs the maker online and reads the file again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_not_hashed_yet_is_fetched_verified_and_its_copy_then_serves_it_by_its_hashes() {
    let film = noise(21_000_000, 3);
    let (a, b, _fa, _fb, id) = owner_and_member(&[("Film.mkv", film.clone()), ("Notes/a.txt", b"note".to_vec())], false).await;
    let got = b.h.od_fetch(&id, "Film.mkv", Duration::from_secs(60)).await.unwrap();
    assert_eq!(std::fs::read(&got.path).unwrap(), film, "the first download, streamed from its maker");

    // A third device, once the maker is gone: only B's copy is left, and B can serve it only chunk by chunk, by the
    // hashes that came signed with the stream.
    let c = device("Gamma").await;
    let fc = tempfile::tempdir().unwrap();
    let link = a.h.create_invite(&id, Role::ReadWrite, 1, 1).unwrap().link;
    c.h.join_folder_on_demand(&link, fc.path().join("Movies")).await.unwrap();
    c.h.add_address(&id, addr(&a)).unwrap();
    c.h.add_address(&id, addr(&b)).unwrap();
    until(30, "C lists the film, and B is connected", || {
        c.h.od_item_at(&id, "Film.mkv").ok().flatten().is_some() && c.h.snapshot().devices.iter().any(|d| d.name == "Beta")
    })
    .await;
    a.h.shutdown().await;
    let mut copy = None;
    for _ in 0..40 {
        match c.h.od_fetch(&id, "Film.mkv", Duration::from_secs(30)).await {
            Ok(f) => {
                copy = Some(f);
                break;
            }
            // (B's index may still be on its way: asked again.)
            Err(_) => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    }
    let copy = copy.expect("C fetched the film from B's copy");
    assert_eq!(std::fs::read(&copy.path).unwrap(), film);
    for d in [&b, &c] {
        d.h.shutdown().await;
    }
}

fn timed(f: impl FnOnce()) -> Duration {
    let t = Instant::now();
    f();
    t.elapsed()
}

/// The failure: a host's listing, item lookup, change feed or the app's status read waits while its device reads and
/// hashes a big file for another device (the Finder's `ls` timed out on 0.1.18 while its folder was busy). Each answers
/// at once, called as the hosts call them, while a slow first download streams: from an owner whose copies are freed
/// (it has a domain too) and from one whose files are in place (its status only).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn listings_and_status_answer_at_once_while_a_file_is_hashed_for_another_device() {
    let film = noise(40_000_000, 5);
    for freed in [true, false] {
        let (a, b, _fa, _fb, id) = owner_and_member(&[("Film.mkv", film.clone()), ("Notes/a.txt", b"note".to_vec())], freed).await;
        // Each piece read after a pause: five pieces, seconds of streaming.
        a.h.test_stream_wait(Duration::from_millis(700));
        let (b2, id2) = (b.h.clone(), id);
        let fetch = tokio::spawn(async move { b2.od_fetch(&id2, "Film.mkv", Duration::from_secs(120)).await });
        let calls = answers_at_once(&a, &b, &id, freed, "the file was streamed", Duration::from_secs(3)).await;
        assert!(!fetch.is_finished(), "the stream was still on its way while the calls were timed");
        assert!(calls > 30, "{calls}");
        a.h.test_stream_wait(Duration::ZERO);
        let got = fetch.await.unwrap().unwrap();
        assert_eq!(std::fs::read(&got.path).unwrap(), film);
        for d in [&a, &b] {
            d.h.shutdown().await;
        }
    }
}

/// The failure: a fetch reads a file's record (not here yet) and what the file needs in two reads, and the pull round
/// that places the file commits between them: the record says not here, the need is gone (it is here now), so no device
/// seems to hold it and the fetch gives up with "not here yet (no device that has it is connected)" just as the file
/// arrives (seen once under load, at the end of a streamed first download, while the host waited up to 120 s). The
/// round's commit lands between the two reads every time here.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fetch_whose_file_is_recorded_between_its_reads_is_not_given_up() {
    let film = noise(3_000_000, 7);
    let (a, b, _fa, _fb, id) = owner_and_member(&[("Film.mkv", film.clone())], false).await;
    // The sessions are up: a fetch no connected device can serve gives up at once.
    b.h.test_fetch_times(Duration::ZERO, Duration::from_secs(30));
    // The piece is read after a pause: the fetch reads the record before the file is here.
    a.h.test_stream_wait(Duration::from_millis(500));
    b.h.test_fetch_read_gap(true);
    let got = b.h.od_fetch(&id, "Film.mkv", Duration::from_secs(60)).await;
    let got = got.unwrap_or_else(|e| panic!("the fetch gave up as its file arrived: {e}"));
    assert_eq!(std::fs::read(&got.path).unwrap(), film);
    for d in [&a, &b] {
        d.h.shutdown().await;
    }
}

/// The failure: a download that came to its end waits for the pull round that places and records it (a round late
/// under load, a slow flush), and a fetch waiting on it meanwhile gives up with "not here yet": the device it came from
/// sends nothing more (it is all here), so it counts as stalled; or that device went away, so none that has it is
/// connected. Asked as Finder and Explorer ask (a short wait, again and again), it answers coming, all of it done,
/// until the record is committed, then here.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_download_waiting_for_its_record_is_coming_until_it_is_recorded() {
    use trs_sync_net::service::FetchWait;
    let film = noise(3_000_000, 8);
    for maker_leaves in [false, true] {
        let (a, b, _fa, _fb, id) = owner_and_member(&[("Film.mkv", film.clone())], false).await;
        // The sessions are up, and a second with no byte further is a stall.
        b.h.test_fetch_times(Duration::ZERO, Duration::from_secs(1));
        // The round that places it comes 3 s after the download's end, and takes 3 s more before it renames and commits.
        b.h.test_place_wait(Duration::from_secs(3));
        let end = Instant::now() + Duration::from_secs(60);
        let (mut whole, mut left) = (0, false);
        loop {
            assert!(Instant::now() < end, "never here (the maker left: {maker_leaves})");
            match b.h.od_fetch_wait(&id, "Film.mkv", Duration::from_millis(300)).await {
                Ok(FetchWait::Here(got)) => {
                    assert_eq!(std::fs::read(&got.path).unwrap(), film);
                    break;
                }
                Ok(FetchWait::Coming { .. }) => {}
                Err(e) => panic!("the fetch gave up while its download waited for its record: {e} (the maker left: {maker_leaves})"),
            }
            let p = b.h.od_progress(&id, "Film.mkv").unwrap();
            if p.done == p.total && !p.here {
                whole += 1;
                if maker_leaves && !left {
                    a.h.shutdown().await;
                    left = true;
                }
            }
        }
        assert!(whole >= 3, "asked only {whole} times while the download waited for its record");
        if !left {
            a.h.shutdown().await;
        }
        b.h.shutdown().await;
    }
}

/// Calls the hosts' and the app's reads for `for_how_long`, as Finder, Explorer and the app call them, and fails if one
/// takes a second or more (`owner_domain`: the owner has a domain, its copies freed). How many calls were made.
async fn answers_at_once(a: &Device, b: &Device, id: &FolderId, owner_domain: bool, during: &str, for_how_long: Duration) -> usize {
    let end = Instant::now() + for_how_long;
    let mut calls = 0;
    while Instant::now() < end {
        let mut all = vec![
            ("status", timed(|| drop(a.h.snapshot()))),
            ("member's children", timed(|| drop(b.h.od_children(id, ROOT_ID, None, 1000).unwrap()))),
            ("member's item", timed(|| drop(b.h.od_item_at(id, "Film.mkv").unwrap()))),
            ("member's changes", timed(|| drop(b.h.od_changes(id, 0, 1000).unwrap()))),
            ("member's anchor", timed(|| { b.h.od_anchor(id).unwrap(); })),
            ("member's progress", timed(|| { b.h.od_progress(id, "Film.mkv").unwrap(); })),
            ("member's status", timed(|| drop(b.h.snapshot()))),
        ];
        if owner_domain {
            all.extend([
                ("children", timed(|| drop(a.h.od_children(id, ROOT_ID, None, 1000).unwrap()))),
                ("item", timed(|| drop(a.h.od_item_at(id, "Film.mkv").unwrap()))),
                ("changes", timed(|| drop(a.h.od_changes(id, 0, 1000).unwrap()))),
                ("anchor", timed(|| { a.h.od_anchor(id).unwrap(); })),
            ]);
        }
        for (what, took) in all {
            assert!(took < Duration::from_secs(1), "{what} took {took:?} while {during}");
            calls += 1;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    calls
}

/// The failure: a listing, item lookup or change feed of an on-demand folder, or the app's status read, waits for its
/// scan (a scan of a big tree, a slow disk: the core held for each step), so the Finder's enumeration or the flyout
/// stalls while a folder scans. Each answers at once while both devices' scans hold their cores for seconds per step.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn listings_and_status_answer_at_once_while_a_scan_holds_the_folder() {
    let film = noise(1_000_000, 6);
    let (a, b, _fa, _fb, id) = owner_and_member(&[("Film.mkv", film), ("Notes/a.txt", b"note".to_vec())], true).await;
    for d in [&a, &b] {
        d.h.test_scan_hold(Duration::from_millis(1500));
        d.h.rescan(&id).unwrap();
    }
    let scanning = |d: &Device| d.h.snapshot().folders.iter().any(|f| f.id == id.to_string() && f.state == trs_sync_net::status::FolderState::Scanning);
    until(10, "both scan", || scanning(&a) && scanning(&b)).await;
    let calls = answers_at_once(&a, &b, &id, true, "the folder scanned", Duration::from_secs(3)).await;
    assert!(calls > 30, "{calls}");
    for d in [&a, &b] {
        d.h.test_scan_hold(Duration::ZERO);
    }
    for d in [&a, &b] {
        d.h.shutdown().await;
    }
}

/// The failure: a full folder's first scan hashes its big files before the other devices hear of anything else, and
/// the app shows a bare spinner meanwhile. Names first: directories and small files are listed on the other device
/// while the big ones are still read, and the scan says how far it is, in files and bytes of the total.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[cfg_attr(windows, ignore = "16 GiB of films: NTFS makes no sparse files")]
async fn a_full_folders_names_come_first_and_its_scan_says_how_far_it_is() {
    let (a, b) = (device("Alpha").await, device("Beta").await);
    let (fa, fb) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let src = fa.path().join("Movies");
    movies(&src, 16, 1 << 30, 50);
    let id: FolderId = a.h.add_folder(src).await.unwrap().id.parse().unwrap();
    let link = a.h.create_invite(&id, Role::ReadWrite, 1, 1).unwrap().link;
    b.h.join_folder_on_demand(&link, fb.path().join("Movies")).await.unwrap();
    b.h.add_address(&id, addr(&a)).unwrap();
    until(20, "B lists the small files while A still hashes", || b.h.od_item_at(&id, "Extras/note 049.txt").ok().flatten().is_some()).await;
    let st = a.h.snapshot().folders.into_iter().find(|f| f.id == id.to_string()).unwrap();
    let scan = st.scan.expect("still scanning: the films are read after the names");
    assert_eq!(scan.phase, trs_sync::scan::ScanPhase::CheckingContents);
    // (The small files were read as the walk met them.)
    assert_eq!((scan.files, scan.check_files), (66, 66), "the totals, known once the walk is over");
    assert!(scan.check_bytes > 16 << 30 && scan.bytes == scan.check_bytes);
    assert!(scan.checked_bytes < scan.check_bytes);
    for d in [&a, &b] {
        d.h.shutdown().await;
    }
}

/// The failure (the receiving side of the user's report): a device that joined an on-demand folder whose owner has
/// not sent its list yet shows a spinner that never ends, its listing waiting for an index that has not come, and
/// says nothing about why. It answers at once (an empty folder), says whom it waits for, and lists the files once
/// they come.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_member_whose_owner_has_not_listed_the_folder_yet_answers_at_once_and_says_whom_it_waits_for() {
    let (a, b) = (device("Alpha").await, device("Beta").await);
    let (fa, fb) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let src = fa.path().join("Movies");
    movies(&src, 2, 1 << 20, 3);
    let id: FolderId = a.h.add_folder_on_demand(src).await.unwrap().id.parse().unwrap();
    // The owner sends its list only after a while (a device busy elsewhere, a slow link).
    a.h.test_announce_pause(&id, Duration::from_secs(4));
    let link = a.h.create_invite(&id, Role::ReadWrite, 1, 1).unwrap().link;
    b.h.join_folder_on_demand(&link, fb.path().join("Movies")).await.unwrap();
    b.h.add_address(&id, addr(&a)).unwrap();
    let waiting = |d: &Device| d.h.snapshot().folders.into_iter().find(|f| f.id == id.to_string()).and_then(|f| f.waiting);
    until(10, "B says whom it waits for", || waiting(&b).is_some_and(|w| w.contains("Alpha") && w.contains("to list"))).await;
    let t = Instant::now();
    let root = b.h.od_children(&id, ROOT_ID, None, 1000).unwrap().unwrap();
    assert!(t.elapsed() < Duration::from_secs(1) && root.items.is_empty(), "an empty folder, at once");
    let t = Instant::now();
    b.h.od_anchor(&id).unwrap();
    b.h.od_changes(&id, 0, 1000).unwrap();
    assert!(t.elapsed() < Duration::from_secs(1));
    a.h.test_announce_pause(&id, Duration::ZERO);
    until(20, "B lists the films once they come", || listed(&b, &id) == 6).await;
    until(5, "and waits for nobody", || waiting(&b).is_none()).await;
    // (The owner never waits for its members' lists: it has its own.)
    until(5, "the owner waits for nobody", || waiting(&a).is_none()).await;
    for d in [&a, &b] {
        d.h.shutdown().await;
    }
}

/// The failure (never hang): a pull round holds the folder's core while it waits for a piece of a stream, which on a
/// slow link (or behind the maker's upload limit) is minutes, and every call the hosts make on the core (a pin, a
/// write, a delete from the Finder or Explorer) and the folder's scan steps wait as long. A stream runs with the core
/// let go: a call on the core answers within about a second while a piece is long on its way.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stream_stalled_on_a_piece_holds_up_no_call_on_the_folders_core() {
    let film = noise(20_000_000, 21);
    let (a, b, _fa, _fb, id) = owner_and_member(&[("Film.mkv", film.clone()), ("Notes/a.txt", b"note".to_vec())], false).await;
    // Each piece read after a pause: the stream stands on a piece for seconds at a time.
    a.h.test_stream_wait(Duration::from_secs(4));
    let (b2, id2) = (b.h.clone(), id);
    let fetch = tokio::spawn(async move { b2.od_fetch(&id2, "Film.mkv", Duration::from_secs(120)).await });
    // The stream under way, then the calls.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    for on in [true, false, true, false] {
        let t = Instant::now();
        b.h.od_pin(&id, "Notes/a.txt", on).await.unwrap();
        assert!(t.elapsed() < Duration::from_secs(1), "a pin waited {:?} for a stream", t.elapsed());
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(!fetch.is_finished(), "the stream was still on its way");
    // (And the folder says it syncs, not that it waits for devices, while the stream runs after the rounds.)
    let state = b.h.snapshot().folders.into_iter().find(|f| f.id == id.to_string()).unwrap().state;
    assert_eq!(state, trs_sync_net::status::FolderState::Syncing);
    a.h.test_stream_wait(Duration::ZERO);
    let got = fetch.await.unwrap().unwrap();
    assert_eq!(std::fs::read(&got.path).unwrap(), film);
    for d in [&a, &b] {
        d.h.shutdown().await;
    }
}

/// The failure: the maker lets go of its read in the middle of a stream (it restarted, the read idled out), answers
/// the next piece busy, and the receiver goes on asking for that piece (or gives the file up). It starts again from
/// the first byte, and the file comes, whole and checked.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_maker_that_let_its_read_go_is_streamed_from_again_from_the_first_byte() {
    let film = noise(40_000_000, 22);
    let (a, b, _fa, _fb, id) = owner_and_member(&[("Film.mkv", film.clone())], false).await;
    a.h.test_stream_wait(Duration::from_millis(600));
    let (b2, id2) = (b.h.clone(), id);
    let fetch = tokio::spawn(async move { b2.od_fetch(&id2, "Film.mkv", Duration::from_secs(120)).await });
    // Some pieces in, then the maker's reads are gone.
    until(30, "the stream is under way", || b.h.od_progress(&id, "Film.mkv").is_ok_and(|p| p.done >= 16 << 20)).await;
    a.h.test_forget_streams();
    a.h.test_stream_wait(Duration::ZERO);
    let got = fetch.await.unwrap().unwrap();
    assert_eq!(std::fs::read(&got.path).unwrap(), film);
    for d in [&a, &b] {
        d.h.shutdown().await;
    }
}

/// The failure (never hang): a pull round holds the folder's core while it waits for a hashed file's chunks from a
/// peer (a minute per chunk at most, a whole film's worth on a slow link), and the hosts' calls on the core and the
/// scan wait as long. The peers' chunks come with the core let go: while a fetch stands on a chunk, a scan of a full
/// folder takes in a new file within a second or two, and a pin on an on-demand one answers within a second.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fetch_stalled_on_a_chunk_holds_up_no_call_on_the_folders_core() {
    let film = noise(12_000_000, 41);
    for on_demand in [false, true] {
        let (a, b) = (device("Alpha").await, device("Beta").await);
        let (fa, fb) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let src = fa.path().join("Movies");
        std::fs::create_dir_all(src.join("Notes")).unwrap();
        std::fs::write(src.join("Film.mkv"), &film).unwrap();
        std::fs::write(src.join("Notes/a.txt"), b"note").unwrap();
        // A full folder: hashed as it is added.
        let id: FolderId = a.h.add_folder(src).await.unwrap().id.parse().unwrap();
        until(30, "A has scanned", || a.h.snapshot().folders.iter().any(|f| f.id == id.to_string() && f.state == trs_sync_net::status::FolderState::UpToDate)).await;
        // Each chunk answered seconds late: a fetch that stands on its chunks.
        a.h.test_serve_wait(Duration::from_secs(4));
        let link = a.h.create_invite(&id, Role::ReadWrite, 1, 1).unwrap().link;
        let place = fb.path().join("Movies");
        if on_demand {
            b.h.join_folder_on_demand(&link, place.clone()).await.unwrap();
        } else {
            b.h.join_folder(&link, place.clone()).await.unwrap();
        }
        b.h.add_address(&id, addr(&a)).unwrap();
        until(30, "B lists the film", || b.h.od_item_at(&id, "Film.mkv").ok().flatten().is_some() || place.join("Notes").exists()).await;
        let fetch = if on_demand {
            let (b2, id2) = (b.h.clone(), id);
            Some(tokio::spawn(async move { b2.od_fetch(&id2, "Film.mkv", Duration::from_secs(120)).await }))
        } else {
            None
        };
        // The fetch standing on its chunks, then the calls.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let case = if on_demand { "on demand" } else { "full" };
        if on_demand {
            for on in [true, false] {
                let t = Instant::now();
                b.h.od_pin(&id, "Notes/a.txt", on).await.unwrap();
                assert!(t.elapsed() < Duration::from_secs(1), "{case}: a pin waited {:?} for a fetch", t.elapsed());
            }
        } else {
            let files = || b.h.snapshot().folders.into_iter().find(|f| f.id == id.to_string()).map_or(0, |f| f.files);
            let before = files();
            std::fs::write(place.join("mine.txt"), b"made on B").unwrap();
            let t = Instant::now();
            b.h.rescan(&id).unwrap();
            until(10, "B's scan takes in its new file", || files() > before).await;
            assert!(t.elapsed() < Duration::from_secs(2), "{case}: a scan waited {:?} for a fetch", t.elapsed());
            assert!(!place.join("Film.mkv").exists(), "{case}: the film was still on its way");
        }
        a.h.test_serve_wait(Duration::ZERO);
        match fetch {
            Some(f) => assert_eq!(std::fs::read(f.await.unwrap().unwrap().path).unwrap(), film),
            None => until(60, "B has the film", || std::fs::read(place.join("Film.mkv")).is_ok_and(|b| b == film)).await,
        }
        for d in [&a, &b] {
            d.h.shutdown().await;
        }
    }
}

/// The failure: the streams of unhashed files and the fetches of hashed ones share one lane, so a stream that takes
/// minutes (a film over a slow link) holds up every hashed file's fetch behind it. Each has its own lane: a small
/// hashed file comes within seconds while the film still streams.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_long_stream_holds_up_no_hashed_files_fetch() {
    let (a, b) = (device("Alpha").await, device("Beta").await);
    let (fa, fb) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let src = fa.path().join("Movies");
    std::fs::create_dir_all(&src).unwrap();
    let notes = noise(300_000, 61);
    std::fs::write(src.join("notes.bin"), &notes).unwrap();
    // A full folder (notes hashed as it is added), then marked on demand for the others: the film added after is
    // listed, not hashed.
    let id: FolderId = a.h.add_folder(src.clone()).await.unwrap().id.parse().unwrap();
    until(30, "A has scanned", || a.h.snapshot().folders.iter().any(|f| f.id == id.to_string() && f.state == trs_sync_net::status::FolderState::UpToDate)).await;
    a.h.set_files_on_demand(&id, true, None).await.unwrap();
    let film = noise(30_000_000, 62);
    std::fs::write(src.join("Film.mkv"), &film).unwrap();
    a.h.rescan(&id).unwrap();
    let link = a.h.create_invite(&id, Role::ReadWrite, 1, 1).unwrap().link;
    b.h.join_folder_on_demand(&link, fb.path().join("Movies")).await.unwrap();
    b.h.add_address(&id, addr(&a)).unwrap();
    until(30, "B lists both", || ["Film.mkv", "notes.bin"].iter().all(|p| b.h.od_item_at(&id, p).ok().flatten().is_some())).await;
    a.h.test_stream_wait(Duration::from_secs(3));
    let (b2, id2) = (b.h.clone(), id);
    let streaming = tokio::spawn(async move { b2.od_fetch(&id2, "Film.mkv", Duration::from_secs(120)).await });
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let t = Instant::now();
    let got = b.h.od_fetch(&id, "notes.bin", Duration::from_secs(60)).await.unwrap();
    assert!(t.elapsed() < Duration::from_secs(3), "the hashed file waited {:?} behind the stream", t.elapsed());
    assert_eq!(std::fs::read(&got.path).unwrap(), notes);
    assert!(!streaming.is_finished(), "the film was still streaming");
    a.h.test_stream_wait(Duration::ZERO);
    assert_eq!(std::fs::read(streaming.await.unwrap().unwrap().path).unwrap(), film);
    for d in [&a, &b] {
        d.h.shutdown().await;
    }
}


/// The failure: a job of a folder's lanes (a fetch of a peer's chunks, run with the folder let go) held the folder's
/// index after the folder was let go: `shutdown` returned while the index was still open, and the service started
/// again on the same state could not open the folder ("the index is already open"; a move or a switch that opens it at
/// its new place met the same). A stop now also tells the jobs to stop, and waits for them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_service_started_again_while_a_fetch_was_finishing_opens_the_folder() {
    let a = device("Alpha").await;
    let (fa, fb, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let start = || async {
        let secrets = Arc::new(FileSecrets::new(state.path().join("secrets")));
        let mut opts = Options::new(state.path().to_path_buf(), "Seedwell test".into(), secrets);
        opts.default_device_name = "Beta".into();
        opts.bind = IpAddr::V4(Ipv4Addr::LOCALHOST);
        opts.lan_discovery = false;
        SyncHandle::start(opts).await.unwrap()
    };
    let src = fa.path().join("Movies");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("notes.bin"), noise(300_000, 71)).unwrap();
    let id: FolderId = a.h.add_folder(src).await.unwrap().id.parse().unwrap();
    until(30, "A has scanned", || a.h.snapshot().folders.iter().any(|f| f.id == id.to_string() && f.state == trs_sync_net::status::FolderState::UpToDate)).await;
    let b = start().await;
    // B's fetch of the file ends, then stands with the index held (as a slow disk's flush would).
    b.test_job_hold(Duration::from_secs(3));
    b.join_folder(&a.h.create_invite(&id, Role::ReadWrite, 1, 1).unwrap().link, fb.path().join("Movies")).await.unwrap();
    b.add_address(&id, addr(&a)).unwrap();
    until(30, "B's fetch holds the index", || b.test_jobs_held() > 0).await;
    b.shutdown().await;
    let b = start().await;
    assert_eq!(b.snapshot().folders.len(), 1, "B's folder opens again right after shutdown");
    b.add_address(&id, addr(&a)).unwrap();
    until(30, "B has the file", || fb.path().join("Movies/notes.bin").exists()).await;
    a.h.shutdown().await;
    b.shutdown().await;
}
