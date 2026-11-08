//! Redundant connections (SPEC 6): a torrent that wants nothing more closes the connections of
//! peers that can never want anything from it, and does not dial them again meanwhile.
use crate::common;
use std::time::Duration;

use common::*;
use seedwell_engine::*;

const S01: [FilePriority; 4] = [FilePriority::Normal, FilePriority::Normal, FilePriority::Skip, FilePriority::Normal];

async fn seeder_of(t: &TestTorrent) -> Node {
    let s = node(|_| {}).await;
    t.write_to(s.dir.path());
    s.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&s, &t.id, 30).await;
    s
}

/// After both let go, neither dials the other again. Counted, not sampled: a connection that comes
/// back and is dropped again within milliseconds still shows. The seed dialling the finished peer
/// straight back was a bug (it saw only the connection end: the HAVE of the last piece and the
/// `upload_only` handshake never went out, so it took the peer for one still downloading). It happened
/// on every run; a sampled peer count saw it about once in 300 runs under load.
async fn no_redial(finished: &Node, seed: &Node) {
    // The seed would dial back as the connection closes; a dial lands in milliseconds.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(finished.h.diagnostics().incoming_accepted, 0, "the seed dialled the finished peer back");
    assert_eq!(seed.h.diagnostics().incoming_accepted, 1, "the finished peer dialled the seed again");
}

/// A complete torrent that just finished downloading from a complete seed: both let go, and the seed
/// does not dial it back (it learns from the peer's `upload_only`, BEP 21, that it wants nothing).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_complete_seeds_disconnect() {
    let t = standard_torrent(3_000_000, None, false, 501);
    let seeder = seeder_of(&t).await;
    let b = node(|_| {}).await;
    b.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();
    wait_done(&b, &t.id, 60).await;
    wait_for("both sides dropped", 20, || async {
        let (x, y) = (b.row(&t.id).await?, seeder.row(&t.id).await?);
        (x.peers == 0 && y.peers == 0).then_some(())
    })
    .await;
    no_redial(&b, &seeder).await;
}

/// A partial seed (every wanted piece held, some files skipped) drops a full seed too, and the seed
/// does not dial it back: it learns from the partial seed's `upload_only` (BEP 21) that it wants nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn partial_seed_disconnects_full_seed() {
    let t = standard_torrent(3_000_000, None, false, 502);
    let seeder = seeder_of(&t).await;
    let p = node(|_| {}).await;
    p.h.add_torrent_file(
        t.bytes.clone(),
        AddOptions { peers: vec![seeder.addr()], file_priorities: S01.to_vec(), ..Default::default() },
    )
    .await
    .unwrap();
    wait_done(&p, &t.id, 60).await;
    wait_for("partial seed dropped the seed", 20, || async {
        (p.row(&t.id).await?.peers == 0 && seeder.row(&t.id).await?.peers == 0).then_some(())
    })
    .await;
    no_redial(&p, &seeder).await;
}

/// A downloading torrent keeps its seeds, and the seed does not dial the downloader's listen address
/// (learned from its extended handshake) while it is connected: that second connection was closed as a
/// duplicate by one side, a wasted dial per peer on every seed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn downloading_torrent_keeps_seeds() {
    let t = standard_torrent(6_000_000, None, false, 503);
    let seeder = seeder_of(&t).await;
    let l = node(|c| c.down_limit = Some(300_000)).await;
    l.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();
    wait_for("connected", 20, || async { (l.row(&t.id).await?.peers == 1).then_some(()) }).await;
    for _ in 0..25 {
        let r = l.row(&t.id).await.unwrap();
        assert_eq!(r.peers, 1, "seed dropped while downloading: {r:?}");
        assert_eq!(r.state, TorrentState::Downloading);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // The seed ticks (and dials) every second while it has a peer: two and a half have passed.
    assert_eq!(l.h.diagnostics().incoming_accepted, 0, "the seed dialled a peer already connected to it");
}

/// After the partial seed drops the full seed, ticking more files dials that address again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ticking_more_files_dials_the_seed_again() {
    let t = standard_torrent(3_000_000, None, false, 504);
    let seeder = seeder_of(&t).await;
    let p = node(|_| {}).await;
    p.h.add_torrent_file(
        t.bytes.clone(),
        AddOptions { peers: vec![seeder.addr()], file_priorities: S01.to_vec(), ..Default::default() },
    )
    .await
    .unwrap();
    wait_done(&p, &t.id, 60).await;
    wait_for("seed dropped", 20, || async { (p.row(&t.id).await?.peers == 0).then_some(()) }).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(p.row(&t.id).await.unwrap().peers, 0, "the seed is not dialled while upload-only");

    p.h.set_file_priorities(t.id, vec![FilePriority::Normal; 4]).await.unwrap();
    wait_for("downloads from the remembered seed", 30, || async {
        let r = p.row(&t.id).await?;
        (r.done_bytes == t.total()).then_some(())
    })
    .await;
}

/// Open a connection that completes the handshake and then says nothing (an idle, silent peer).
async fn silent_peer(addr: std::net::SocketAddr, t: &TestTorrent, tag: u8) -> tokio::net::TcpStream {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut hs = vec![19u8];
    hs.extend(b"BitTorrent protocol");
    hs.extend([0u8; 8]);
    hs.extend(t.id);
    let mut pid = *b"-XX0001-silentsilent";
    pid[19] = tag;
    hs.extend(pid);
    s.write_all(&hs).await.unwrap();
    let mut theirs = [0u8; 68];
    s.read_exact(&mut theirs).await.unwrap();
    s
}

/// An upload-only torrent whose slots are taken by idle peers still admits a newcomer: the idle
/// ones are evicted instead of the leecher being refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn upload_only_torrent_evicts_idle_peers_for_a_newcomer() {
    let t = standard_torrent(2_000_000, None, false, 505);
    let seeder = node(|c| c.max_connections_per_torrent = 2).await;
    t.write_to(seeder.dir.path());
    seeder.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&seeder, &t.id, 30).await;
    let _a = silent_peer(seeder.addr(), &t, b'a').await;
    let _b = silent_peer(seeder.addr(), &t, b'b').await;
    wait_for("both slots taken", 10, || async { (seeder.row(&t.id).await?.peers == 2).then_some(()) }).await;
    tokio::time::sleep(Duration::from_millis(10_500)).await;

    let l = node(|_| {}).await;
    l.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();
    wait_done(&l, &t.id, 30).await;
    t.verify(l.dir.path(), &[]);
}

/// A peer that is still downloading (it keeps announcing HAVEs, from anyone) is not evicted for a
/// newcomer even though it asks nothing of us right now: it stays until it is done.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_peer_still_downloading_is_not_evicted() {
    use tokio::io::AsyncWriteExt;
    let t = standard_torrent(2_000_000, None, false, 507);
    let seeder = node(|c| c.max_connections_per_torrent = 2).await;
    t.write_to(seeder.dir.path());
    seeder.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&seeder, &t.id, 30).await;
    let mut a = silent_peer(seeder.addr(), &t, b'a').await;
    let mut b = silent_peer(seeder.addr(), &t, b'b').await;
    wait_for("both slots taken", 10, || async { (seeder.row(&t.id).await?.peers == 2).then_some(()) }).await;
    tokio::time::sleep(Duration::from_millis(10_500)).await;
    // HAVE piece 0: each got a piece from someone just now.
    for s in [&mut a, &mut b] {
        s.write_all(&[0, 0, 0, 5, 4, 0, 0, 0, 0]).await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let refused = seeder.h.diagnostics().incoming_refused;
    // The newcomer may be closed before or after the handshake reply: don't insist on one.
    let mut c = tokio::net::TcpStream::connect(seeder.addr()).await.unwrap();
    let mut hs = vec![19u8];
    hs.extend(b"BitTorrent protocol");
    hs.extend([0u8; 8]);
    hs.extend(t.id);
    hs.extend(*b"-XX0001-silentsilenc");
    let _ = c.write_all(&hs).await;
    wait_for("newcomer refused", 10, || async { (seeder.h.diagnostics().incoming_refused > refused).then_some(()) }).await;
    assert_eq!(seeder.row(&t.id).await.unwrap().peers, 2, "a downloading peer was evicted");
}

/// A tracker that reports no leechers: the seeding torrent dials nobody (neither of the two
/// seeds ever connects to the other), and no connection stays open.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seed_dials_nobody_when_the_tracker_reports_no_leechers() {
    let tracker = MockTracker::start(60).await;
    let t = standard_torrent(1_000_000, Some(tracker.url()), false, 506);
    let mut nodes = Vec::new();
    for _ in 0..2 {
        let n = node(|c| c.trackers_enabled = true).await;
        t.write_to(n.dir.path());
        n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
        wait_done(&n, &t.id, 30).await;
        let want = nodes.len() + 1;
        wait_for("announced", 10, || async { (tracker.log_for(&t.id).len() >= want).then_some(()) }).await;
        nodes.push(n);
    }
    tokio::time::sleep(Duration::from_millis(3000)).await;
    for n in &nodes {
        let d = n.h.diagnostics();
        assert_eq!(d.incoming_accepted, 0, "a seed was dialled");
        assert_eq!(n.row(&t.id).await.unwrap().peers, 0);
    }
}

/// The id our simulated libtorrent peer gives its `upload_only` message.
const LT_UPLOAD_ONLY: u8 = 7;

/// How a libtorrent-family peer (qBittorrent, Deluge) sees our `upload_only`, from libtorrent RC_2_0's
/// `bt_peer_connection.cpp`: an extended handshake that says it sets it (`on_extended_handshake` calls
/// `set_upload_only(true)` alone, never `false`); only lt's `upload_only` message (one byte) clears it.
#[derive(Default)]
struct LtView {
    sees_upload_only: bool,
    /// lt `upload_only` messages received, in order.
    messages: Vec<bool>,
}

impl LtView {
    /// Read one frame and apply it. `Ok(false)`: the stream ended cleanly (a FIN).
    async fn read_frame(&mut self, s: &mut (impl tokio::io::AsyncRead + Unpin)) -> std::io::Result<bool> {
        use tokio::io::AsyncReadExt;
        let mut len = [0u8; 4];
        match s.read_exact(&mut len).await {
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(false),
            r => r?,
        };
        let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
        s.read_exact(&mut body).await?;
        if let [20, ext, payload @ ..] = &body[..] {
            match *ext {
                0 => {
                    if seedwell_wire::ExtendedHandshake::parse(payload).unwrap().upload_only == Some(true) {
                        self.sees_upload_only = true;
                    }
                }
                LT_UPLOAD_ONLY => {
                    assert_eq!(payload.len(), 1, "libtorrent ignores an upload_only message that is not one byte");
                    self.sees_upload_only = payload[0] != 0;
                    self.messages.push(self.sees_upload_only);
                }
                _ => {}
            }
        }
        Ok(true)
    }

    /// Read until the peer sees us as `want`.
    async fn until(&mut self, s: &mut tokio::net::TcpStream, what: &str, want: bool) {
        let read = async {
            while self.sees_upload_only != want {
                assert!(self.read_frame(s).await.unwrap(), "{what}: the connection ended");
            }
        };
        tokio::time::timeout(Duration::from_secs(15), read).await.unwrap_or_else(|_| panic!("timed out: {what}"));
    }
}

/// Connect as a libtorrent-like peer: BitTorrent handshake with the extension bit, then an extended
/// handshake that lists `upload_only` in `m` (and says `upload_only` itself when `seed`).
async fn lt_peer(addr: std::net::SocketAddr, t: &TestTorrent, seed: bool) -> tokio::net::TcpStream {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut hs = vec![19u8];
    hs.extend(b"BitTorrent protocol");
    hs.extend([0, 0, 0, 0, 0, 0x10, 0, 0]);
    hs.extend(t.id);
    hs.extend(*b"-XX0001-ltsimulated0");
    s.write_all(&hs).await.unwrap();
    let mut theirs = [0u8; 68];
    tokio::time::timeout(Duration::from_secs(10), s.read_exact(&mut theirs)).await.unwrap().unwrap();
    let ext = format!("d1:md11:upload_onlyi{LT_UPLOAD_ONLY}ee{}e", if seed { "11:upload_onlyi1e" } else { "" });
    let mut f = (2 + ext.len() as u32).to_be_bytes().to_vec();
    f.extend([20, 0]);
    f.extend(ext.as_bytes());
    s.write_all(&f).await.unwrap();
    s
}

/// A partial seed that wants pieces again (more files ticked) tells a libtorrent peer so with lt's
/// `upload_only` message. Its re-sent handshake alone is not enough: libtorrent never clears
/// `upload_only` from a handshake, so the peer kept taking it for upload-only and later dropped it as
/// redundant while it was downloading.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_libtorrent_peer_sees_a_partial_seed_want_pieces_again() {
    let t = standard_torrent(3_000_000, None, false, 508);
    let seeder = seeder_of(&t).await;
    let p = node(|_| {}).await;
    p.h.add_torrent_file(
        t.bytes.clone(),
        AddOptions { peers: vec![seeder.addr()], file_priorities: S01.to_vec(), ..Default::default() },
    )
    .await
    .unwrap();
    wait_done(&p, &t.id, 60).await;

    // A downloader with nothing yet: the partial seed keeps it, and its handshake says upload-only.
    let mut s = lt_peer(p.addr(), &t, false).await;
    let mut lt = LtView::default();
    lt.until(&mut s, "the partial seed says it is upload-only", true).await;

    p.h.set_file_priorities(t.id, vec![FilePriority::Normal; 4]).await.unwrap();
    lt.until(&mut s, "the libtorrent peer sees the partial seed want pieces again", false).await;
    assert_eq!(lt.messages.last(), Some(&false));
}

/// A peer dropped as redundant gets everything queued for it, then a clean end, even when it keeps
/// sending after our FIN (a half-closed connection may): the leaving side reads until the peer closes
/// too. Closed with the peer's bytes unread, or reached by them after it closed, a socket answers with
/// a reset, and on Windows a reset discards what the peer has received and not yet read: the handshake
/// that says we want nothing, the HAVE of the last piece.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_peer_left_as_redundant_gets_a_clean_end_while_it_still_sends() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::io::AsyncWriteExt;
    let t = standard_torrent(1_000_000, None, false, 509);
    let seeder = seeder_of(&t).await;
    // Another seed (its handshake says upload_only): redundant to the seeder, which leaves at once.
    let s = lt_peer(seeder.addr(), &t, true).await;
    let (mut r, mut w) = s.into_split();
    let fin = std::sync::Arc::new(AtomicBool::new(false));
    let fin2 = fin.clone();
    let reader = tokio::spawn(async move {
        let mut lt = LtView::default();
        let end = loop {
            match lt.read_frame(&mut r).await {
                Ok(true) => continue,
                Ok(false) => break Ok(()),
                Err(e) => break Err(e),
            }
        };
        fin2.store(true, Ordering::Relaxed);
        (lt.sees_upload_only, end)
    });
    // Keep-alives until a while after the FIN (or the end of the connection, whatever it is).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut after_fin: Option<tokio::time::Instant> = None;
    while tokio::time::Instant::now() < deadline {
        if fin.load(Ordering::Relaxed) && after_fin.is_none() {
            after_fin = Some(tokio::time::Instant::now() + Duration::from_millis(300));
        }
        if after_fin.is_some_and(|a| tokio::time::Instant::now() >= a) {
            break;
        }
        if let Err(e) = w.write_all(&[0, 0, 0, 0]).await {
            panic!("the seed reset the connection while the peer still sent ({e})");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(after_fin.is_some(), "the seed never closed the connection");
    drop(w);
    let (said, end) = reader.await.unwrap();
    if let Err(e) = end {
        panic!("the connection ended in an error, not a FIN: {e}");
    }
    assert!(said, "the handshake saying the seed wants nothing did not arrive");
    // The seed's peer task has ended (the peer closed its side): the connection is gone there too.
    wait_for("the seed lets go", 10, || async { (seeder.row(&t.id).await?.peers == 0).then_some(()) }).await;
}
