//! A seeder serving raw BitTorrent leechers the way Transmission and libtorrent behave: the
//! regression test for the failed "hot" benchmark (`bench/scripts/tests/repro-seeder-reconnect.py`).
//!
//! The leecher here is hand-written wire code (no engine on that side): it says `interested`
//! late, pipelines up to 200 requests, reads whole pieces, verifies every byte and then drops the
//! connection without a goodbye. A second, *new* leecher from the same IP (new peer id, new
//! ephemeral port) must be unchoked at once and served at full speed.
//!
//! Root causes covered (see the engine README, "Seeding performance"):
//! * a torrent that had a rechoke with nobody interested held 0 upload slots, so the first
//!   `interested` waited for the next rechoke (up to 10 s) before it was unchoked;
//! * one storage read and one thread hand-off per 16 KiB block;
//! * (benchmark client only) the default rule stops a torrent at ratio 1.0, and a stopped torrent
//!   refuses every handshake: see `stopped_torrent_refuses_new_leechers`.
use crate::common;
use std::time::{Duration, Instant};

use common::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use trs_engine::*;

const BLOCK: u32 = 16 * 1024;

/// What one raw leecher saw.
struct Leech {
    /// `interested` sent until the first `unchoke`.
    unchoke_after: Duration,
    /// First request until the last byte.
    transfer: Duration,
    bytes: u64,
}

impl Leech {
    fn mb_s(&self) -> f64 {
        self.bytes as f64 / 1e6 / self.transfer.as_secs_f64().max(1e-6)
    }
}

async fn read_msg(s: &mut TcpStream, buf: &mut Vec<u8>) -> std::io::Result<Option<(u8, usize)>> {
    let mut len = [0u8; 4];
    s.read_exact(&mut len).await?;
    let n = u32::from_be_bytes(len) as usize;
    if n == 0 {
        return Ok(None);
    }
    buf.resize(n, 0);
    s.read_exact(&mut buf[..n]).await?;
    Ok(Some((buf[0], n)))
}

/// Connect, handshake (Fast + extension protocol, like Transmission), wait `quiet`, say
/// `interested`, wait for the unchoke, then download the whole torrent with a deep pipeline.
/// With `abort_after`, drop the socket after that many bytes instead (a leecher that crashes).
async fn leech(addr: std::net::SocketAddr, t: &TestTorrent, peer_tag: u8, quiet: Duration, abort_after: Option<u64>) -> Leech {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.set_nodelay(true).unwrap();
    let mut hs = Vec::with_capacity(68);
    hs.push(19);
    hs.extend(b"BitTorrent protocol");
    let mut reserved = [0u8; 8];
    reserved[5] |= 0x10; // extension protocol
    reserved[7] |= 0x04; // Fast
    hs.extend(reserved);
    hs.extend(t.id);
    let mut pid = *b"-TR4130-xxxxxxxxxxxx";
    pid[19] = peer_tag;
    hs.extend(pid);
    s.write_all(&hs).await.unwrap();
    let mut theirs = [0u8; 68];
    s.read_exact(&mut theirs).await.unwrap();
    assert_eq!(&theirs[28..48], &t.id);

    // The seeder talks first (HaveAll, extended handshake); we just wait like a busy client.
    let mut buf = Vec::new();
    let quiet_end = Instant::now() + quiet;
    loop {
        let left = quiet_end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        match tokio::time::timeout(left, read_msg(&mut s, &mut buf)).await {
            Ok(r) => {
                r.unwrap();
            }
            Err(_) => break,
        }
    }
    s.write_all(&[0, 0, 0, 1, 2]).await.unwrap(); // interested
    let asked = Instant::now();
    let unchoke_after = loop {
        if let Some((id, _)) = tokio::time::timeout(Duration::from_secs(20), read_msg(&mut s, &mut buf))
            .await
            .expect("no unchoke within 20 s")
            .unwrap()
            && id == 1 {
                break asked.elapsed();
            }
    };

    // Expected stream for verification.
    let mut stream = Vec::with_capacity(t.total() as usize);
    for (_, d) in &t.files {
        stream.extend_from_slice(d);
    }
    let total = stream.len() as u64;
    let piece_len = u64::from(t.piece_len);
    let mut blocks: Vec<(u32, u32, u32)> = Vec::new();
    for p in 0..t.pieces {
        let start = u64::from(p) * piece_len;
        let size = piece_len.min(total - start) as u32;
        let mut b = 0;
        while b < size {
            let l = BLOCK.min(size - b);
            blocks.push((p, b, l));
            b += l;
        }
    }
    let started = Instant::now();
    let (mut sent, mut got, mut bytes) = (0usize, 0usize, 0u64);
    const WINDOW: usize = 200;
    let mut out = Vec::new();
    while got < blocks.len() {
        out.clear();
        while sent < blocks.len() && sent - got < WINDOW {
            let (p, b, l) = blocks[sent];
            out.extend(13u32.to_be_bytes());
            out.push(6);
            out.extend(p.to_be_bytes());
            out.extend(b.to_be_bytes());
            out.extend(l.to_be_bytes());
            sent += 1;
        }
        if !out.is_empty() {
            s.write_all(&out).await.unwrap();
        }
        let msg = tokio::time::timeout(Duration::from_secs(20), read_msg(&mut s, &mut buf))
            .await
            .unwrap_or_else(|_| panic!("stalled at block {got} of {}", blocks.len()))
            .unwrap();
        let Some((id, n)) = msg else { continue };
        if id != 7 {
            continue; // have, extended handshake, ...
        }
        let p = u32::from_be_bytes(buf[1..5].try_into().unwrap());
        let b = u32::from_be_bytes(buf[5..9].try_into().unwrap());
        let data = &buf[9..n];
        let (ep, eb, el) = blocks[got];
        assert_eq!((p, b, data.len() as u32), (ep, eb, el), "blocks must come back in request order");
        let at = (u64::from(p) * piece_len + u64::from(b)) as usize;
        assert!(data == &stream[at..at + data.len()], "block {p}/{b} differs");
        bytes += data.len() as u64;
        got += 1;
        if abort_after.is_some_and(|a| bytes >= a) {
            break; // drop: RST/FIN without a word
        }
    }
    Leech { unchoke_after, transfer: started.elapsed(), bytes }
}

/// Two leecher processes in a row from one IP: both are unchoked within a second although the
/// torrent had been idle through several rechokes, and both get the data at speed. Also covers a
/// first leecher that dies in the middle of a transfer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn second_leecher_from_the_same_ip_is_served_fast() {
    let seeder = node(|c| {
        // The first rechoke (nobody interested yet) must have run before `interested` arrives.
        c.rechoke_interval = Duration::from_secs(2);
    })
    .await;
    // 24 MiB in 256 KiB pieces, a big file plus a small one.
    let t = make_torrent(&TorrentSpec {
        name: "hot.bin",
        piece_len: 256 * 1024,
        files: &[("hot.bin", 24 << 20)],
        private: false,
        announce: None,
        seed: 77,
    });
    t.write_to(seeder.dir.path());
    // A seeder with many other (idle) torrents, as in the benchmark.
    let mut others = Vec::new();
    for i in 0..30u64 {
        let o = make_torrent(&TorrentSpec {
            name: &format!("idle{i}.bin"),
            piece_len: 32 * 1024,
            files: &[(&format!("idle{i}.bin"), 200_000)],
            private: false,
            announce: None,
            seed: 1000 + i,
        });
        o.write_to(seeder.dir.path());
        others.push(o);
    }
    for o in &others {
        seeder.h.add_torrent_file(o.bytes.clone(), AddOptions::default()).await.unwrap();
    }
    seeder.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&seeder, &t.id, 60).await;
    for o in &others {
        wait_done(&seeder, &o.id, 60).await;
    }
    let addr = seeder.addr();

    // Leecher A dies after ~6 MiB.
    let a = leech(addr, &t, b'A', Duration::from_secs(3), Some(6 << 20)).await;
    eprintln!("A: unchoke after {:?}, {:.0} MB/s", a.unchoke_after, a.mb_s());
    assert!(a.unchoke_after < Duration::from_secs(1), "first unchoke took {:?}", a.unchoke_after);

    // The seeder notices the dead peer.
    wait_for("seeder drops the dead leecher", 20, || async {
        (seeder.row(&t.id).await?.peers == 0).then_some(())
    })
    .await;

    // Leecher B, a new process from the same IP.
    let b = leech(addr, &t, b'B', Duration::from_secs(3), None).await;
    eprintln!("B: unchoke after {:?}, {:.0} MB/s", b.unchoke_after, b.mb_s());
    assert!(b.unchoke_after < Duration::from_secs(1), "second unchoke took {:?}", b.unchoke_after);
    assert_eq!(b.bytes, t.total());
    // The floor is far below what a release build reaches (hundreds of MB/s); it only catches
    // a regression to one-block-at-a-time serving or a starved upload slot.
    assert!(b.mb_s() > 25.0, "leecher B got {:.1} MB/s", b.mb_s());

    // And B's death leaves the torrent seeding, ready for C (two connections in parallel).
    wait_for("seeder drops B", 20, || async { (seeder.row(&t.id).await?.peers == 0).then_some(()) }).await;
    let (c1, c2) = tokio::join!(
        leech(addr, &t, b'C', Duration::from_millis(100), None),
        leech(addr, &t, b'D', Duration::from_millis(100), None)
    );
    eprintln!("C: {:?} {:.0} MB/s, D: {:?} {:.0} MB/s", c1.unchoke_after, c1.mb_s(), c2.unchoke_after, c2.mb_s());
    assert_eq!(c1.bytes, t.total());
    assert_eq!(c2.bytes, t.total());
    // Two leechers arriving together both get a slot at once, not at the next rechoke.
    assert!(c1.unchoke_after < Duration::from_secs(1) && c2.unchoke_after < Duration::from_secs(1));
    let row = seeder.row(&t.id).await.unwrap();
    assert!(row.uploaded_total >= 3 * t.total(), "uploaded {}", row.uploaded_total);
    assert!(matches!(row.state, TorrentState::Seeding | TorrentState::LowPower), "state {:?}", row.state);
}

/// What Transmission leechers pipeline is capped by the `reqq` we advertise and re-filled twice a
/// second, so `reqq` is the ceiling of one connection (250 capped the hot benchmark at 122 MB/s, 1000
/// gets ~200 MB/s from the same three leechers). The leecher here believes the advertised `reqq`:
/// it reads it from our extended handshake and keeps exactly that many requests in flight, in
/// bursts (all at once, then wait for the lot, like a periodic refill). A seeder that dropped the
/// connection at its own queue limit, or served the burst slowly, fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_leecher_that_fills_the_advertised_queue_is_served() {
    let seeder = node(|_| {}).await;
    let t = make_torrent(&TorrentSpec {
        name: "deep.bin",
        piece_len: 256 * 1024,
        files: &[("deep.bin", 48 << 20)],
        private: false,
        announce: None,
        seed: 91,
    });
    t.write_to(seeder.dir.path());
    seeder.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&seeder, &t.id, 60).await;

    let mut s = TcpStream::connect(seeder.addr()).await.unwrap();
    s.set_nodelay(true).unwrap();
    let mut hs = vec![19];
    hs.extend(b"BitTorrent protocol");
    let mut reserved = [0u8; 8];
    reserved[5] |= 0x10;
    reserved[7] |= 0x04;
    hs.extend(reserved);
    hs.extend(t.id);
    hs.extend(*b"-TR4130-deepdeepdeep");
    s.write_all(&hs).await.unwrap();
    let mut theirs = [0u8; 68];
    s.read_exact(&mut theirs).await.unwrap();

    // Our extended handshake (empty `m`), then read theirs for `reqq`.
    let payload = b"d1:mdee";
    let mut ext = ((payload.len() + 2) as u32).to_be_bytes().to_vec();
    ext.extend([20, 0]);
    ext.extend(payload);
    s.write_all(&ext).await.unwrap();
    let mut buf = Vec::new();
    let mut reqq = None;
    let mut unchoked = false;
    s.write_all(&[0, 0, 0, 1, 2]).await.unwrap(); // interested
    let until = Instant::now() + Duration::from_secs(10);
    while (reqq.is_none() || !unchoked) && Instant::now() < until {
        let Ok(Ok(Some((id, n)))) = tokio::time::timeout(Duration::from_secs(5), read_msg(&mut s, &mut buf)).await else {
            continue;
        };
        match id {
            1 => unchoked = true,
            20 if buf[1] == 0 => {
                let d = &buf[2..n];
                let pos = d.windows(7).position(|w| w == b"4:reqqi").expect("no reqq in the extended handshake");
                let digits: Vec<u8> = d[pos + 7..].iter().copied().take_while(u8::is_ascii_digit).collect();
                reqq = Some(String::from_utf8(digits).unwrap().parse::<usize>().unwrap());
            }
            _ => {}
        }
    }
    let reqq = reqq.expect("no extended handshake");
    assert!(unchoked, "not unchoked");
    assert!(reqq >= 512, "advertised reqq {reqq} caps a Transmission leecher below its speed");

    let mut stream = Vec::with_capacity(t.total() as usize);
    for (_, d) in &t.files {
        stream.extend_from_slice(d);
    }
    let piece_len = u64::from(t.piece_len);
    let total = stream.len() as u64;
    let mut blocks = Vec::new();
    for p in 0..t.pieces {
        let size = piece_len.min(total - u64::from(p) * piece_len) as u32;
        let mut b = 0;
        while b < size {
            blocks.push((p, b, BLOCK.min(size - b)));
            b += BLOCK.min(size - b);
        }
    }
    let started = Instant::now();
    let (mut got, mut bytes) = (0usize, 0u64);
    while got < blocks.len() {
        let burst = &blocks[got..blocks.len().min(got + reqq)];
        let mut out = Vec::with_capacity(burst.len() * 17);
        for &(p, b, l) in burst {
            out.extend(13u32.to_be_bytes());
            out.push(6);
            out.extend(p.to_be_bytes());
            out.extend(b.to_be_bytes());
            out.extend(l.to_be_bytes());
        }
        s.write_all(&out).await.unwrap();
        for _ in 0..burst.len() {
            loop {
                let msg = tokio::time::timeout(Duration::from_secs(20), read_msg(&mut s, &mut buf))
                    .await
                    .unwrap_or_else(|_| panic!("stalled at block {got} of {}", blocks.len()))
                    .unwrap_or_else(|e| panic!("connection closed at block {got}: {e}"));
                let Some((7, n)) = msg else { continue };
                let p = u32::from_be_bytes(buf[1..5].try_into().unwrap());
                let b = u32::from_be_bytes(buf[5..9].try_into().unwrap());
                let (ep, eb, el) = blocks[got];
                assert_eq!((p, b, (n - 9) as u32), (ep, eb, el), "blocks must come back in request order");
                let at = (u64::from(p) * piece_len + u64::from(b)) as usize;
                assert!(buf[9..n] == stream[at..at + n - 9], "block {p}/{b} differs");
                bytes += (n - 9) as u64;
                got += 1;
                break;
            }
        }
    }
    let mb_s = bytes as f64 / 1e6 / started.elapsed().as_secs_f64().max(1e-6);
    eprintln!("deep queue (reqq {reqq}): {bytes} bytes, {mb_s:.0} MB/s");
    assert_eq!(bytes, t.total());
    assert!(mb_s > 25.0, "got {mb_s:.1} MB/s");
}

/// The failure mode of the first benchmark campaign, as a documented behaviour: once the seeding
/// goal is reached the torrent is stopped, and a stopped torrent refuses new leechers. The
/// benchmark client therefore sets the goal to "forever".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stopped_torrent_refuses_new_leechers() {
    let seeder = node(|_| {}).await;
    let t = make_torrent(&TorrentSpec {
        name: "goal.bin",
        piece_len: 64 * 1024,
        files: &[("goal.bin", 2 << 20)],
        private: false,
        announce: None,
        seed: 5,
    });
    t.write_to(seeder.dir.path());
    let opts = AddOptions {
        seed_goal: Some(SeedGoal { ratio: Some(1.0), ..SeedGoal::FOREVER }),
        ..AddOptions::default()
    };
    seeder.h.add_torrent_file(t.bytes.clone(), opts).await.unwrap();
    wait_done(&seeder, &t.id, 30).await;
    let first = leech(seeder.addr(), &t, b'A', Duration::from_millis(50), None).await;
    assert_eq!(first.bytes, t.total());
    wait_for("goal reached, torrent stopped", 30, || async {
        (seeder.state(&t.id).await? == TorrentState::Paused).then_some(())
    })
    .await;
    let mut s = TcpStream::connect(seeder.addr()).await.unwrap();
    let mut hs = vec![19u8];
    hs.extend(b"BitTorrent protocol");
    hs.extend([0u8; 8]);
    hs.extend(t.id);
    hs.extend(*b"-TR4130-yyyyyyyyyyyy");
    s.write_all(&hs).await.unwrap();
    let mut buf = [0u8; 68];
    let r = tokio::time::timeout(Duration::from_secs(5), s.read_exact(&mut buf)).await;
    assert!(!matches!(r, Ok(Ok(_))), "a stopped torrent must not answer a handshake");
}

/// SPEC 2: Cold to serving the first block under 50 ms. A Cold torrent has no actor and no layout;
/// the first handshake wakes it (startup, resume check) and the first block must follow at once.
/// The assertion leaves room for a debug build on a busy machine; the printed number is the
/// measurement (a few milliseconds).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cold_torrent_serves_its_first_block_fast() {
    let seeder = node(|c| {
        c.tiers = TierTimings {
            hot_to_warm: Duration::from_millis(100),
            warm_to_cold: Duration::from_millis(300),
            ..TierTimings::default()
        };
    })
    .await;
    let t = standard_torrent(2_000_000, None, false, 61);
    t.write_to(seeder.dir.path());
    seeder.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&seeder, &t.id, 30).await;
    wait_for("the torrent went Cold", 30, || async { (seeder.row(&t.id).await?.tier == Tier::Cold).then_some(()) }).await;

    let started = Instant::now();
    let mut s = TcpStream::connect(seeder.addr()).await.unwrap();
    s.set_nodelay(true).unwrap();
    let mut hs = vec![19u8];
    hs.extend(b"BitTorrent protocol");
    let mut reserved = [0u8; 8];
    reserved[7] |= 0x04;
    hs.extend(reserved);
    hs.extend(t.id);
    hs.extend(*b"-TR4130-coldcoldcold");
    s.write_all(&hs).await.unwrap();
    let mut theirs = [0u8; 68];
    s.read_exact(&mut theirs).await.unwrap();
    s.write_all(&[0, 0, 0, 1, 2]).await.unwrap(); // interested
    let mut buf = Vec::new();
    loop {
        if let Some((1, _)) = read_msg(&mut s, &mut buf).await.unwrap() {
            break;
        }
    }
    let mut req = vec![0, 0, 0, 13, 6];
    req.extend(0u32.to_be_bytes());
    req.extend(0u32.to_be_bytes());
    req.extend(BLOCK.to_be_bytes());
    s.write_all(&req).await.unwrap();
    loop {
        if let Some((7, _)) = read_msg(&mut s, &mut buf).await.unwrap() {
            break;
        }
    }
    let took = started.elapsed();
    eprintln!("Cold torrent: connect to first block in {took:?}");
    assert!(took < Duration::from_millis(250), "{took:?}");
}

/// Some clients ask for blocks bigger than 16 KiB. They are served (up to 128 KiB, like libtorrent);
/// the wire decoder once hung up on them before the request reached the torrent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_request_for_a_32_kib_block_is_served() {
    let seeder = node(|_| {}).await;
    let t = standard_torrent(2_000_000, None, false, 63);
    t.write_to(seeder.dir.path());
    seeder.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&seeder, &t.id, 30).await;
    let mut s = TcpStream::connect(seeder.addr()).await.unwrap();
    let mut hs = vec![19u8];
    hs.extend(b"BitTorrent protocol");
    hs.extend([0u8; 8]);
    hs.extend(t.id);
    hs.extend(*b"-TR4130-bigblockbig1");
    s.write_all(&hs).await.unwrap();
    let mut theirs = [0u8; 68];
    s.read_exact(&mut theirs).await.unwrap();
    s.write_all(&[0, 0, 0, 1, 2]).await.unwrap(); // interested
    let mut buf = Vec::new();
    while !matches!(read_msg(&mut s, &mut buf).await.unwrap(), Some((1, _))) {}
    let mut req = vec![0, 0, 0, 13, 6];
    req.extend(0u32.to_be_bytes());
    req.extend(0u32.to_be_bytes());
    req.extend((32u32 * 1024).to_be_bytes());
    s.write_all(&req).await.unwrap();
    let n = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some((7, n)) = read_msg(&mut s, &mut buf).await.expect("the seeder hung up") {
                return n;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(n, 9 + 32 * 1024);
    assert_eq!(&buf[9..n], &t.files[0].1[..32 * 1024]);
}

/// A Cold torrent on a disk that has to spin up takes seconds to start. The peer that woke it, and
/// every peer arriving meanwhile, gets our handshake at once (clients give up after about 10 s and
/// back off for minutes) and is served once the torrent is ready. Peers arriving during the startup
/// were once closed without a word.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn peers_reaching_a_waking_torrent_are_answered_at_once_and_served() {
    let seeder = node(|c| {
        c.tiers = TierTimings {
            hot_to_warm: Duration::from_millis(100),
            warm_to_cold: Duration::from_millis(300),
            ..TierTimings::default()
        };
        c.test_startup_delay = Duration::from_secs(3);
    })
    .await;
    let t = standard_torrent(2_000_000, None, false, 62);
    t.write_to(seeder.dir.path());
    seeder.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&seeder, &t.id, 30).await;
    wait_for("the torrent went Cold", 30, || async { (seeder.row(&t.id).await?.tier == Tier::Cold).then_some(()) }).await;

    async fn hello(addr: std::net::SocketAddr, ih: [u8; 20], tag: u8) -> (TcpStream, Duration) {
        let started = Instant::now();
        let mut s = TcpStream::connect(addr).await.unwrap();
        s.set_nodelay(true).unwrap();
        let mut hs = vec![19u8];
        hs.extend(b"BitTorrent protocol");
        hs.extend([0u8; 8]);
        hs.extend(ih);
        hs.extend(*b"-TR4130-wakewakewak");
        hs.push(tag);
        s.write_all(&hs).await.unwrap();
        let mut theirs = [0u8; 68];
        tokio::time::timeout(Duration::from_secs(10), s.read_exact(&mut theirs)).await.expect("no handshake").unwrap();
        assert_eq!(&theirs[28..48], &ih);
        (s, started.elapsed())
    }
    let (mut a, took_a) = hello(seeder.addr(), t.id, b'a').await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (mut b, took_b) = hello(seeder.addr(), t.id, b'b').await;
    eprintln!("handshakes answered in {took_a:?} and {took_b:?}");
    assert!(took_a < Duration::from_secs(1) && took_b < Duration::from_secs(1), "{took_a:?} {took_b:?}");
    let refused = seeder.h.diagnostics().incoming_refused;
    for s in [&mut a, &mut b] {
        s.write_all(&[0, 0, 0, 1, 2]).await.unwrap(); // interested
        let mut buf = Vec::new();
        tokio::time::timeout(Duration::from_secs(15), async {
            while !matches!(read_msg(s, &mut buf).await.unwrap(), Some((1, _))) {}
        })
        .await
        .expect("not unchoked once the torrent was up");
    }
    assert_eq!(seeder.h.diagnostics().incoming_refused, refused);
}

/// A seeder is dialled by leechers; it does not chase every address a tracker or the DHT hands it.
/// Forty silent hosts (accept, never answer) were once enough to hold 32 connection slots for
/// as long as the torrent seeded, with nothing connected: now at most two attempts are in flight
/// and a new one is earned every 30 s, and the connection count shows only what is connected.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_seeder_does_not_chase_every_address() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let accepted = Arc::new(AtomicUsize::new(0));
    let mut hosts = Vec::new();
    for _ in 0..40 {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        hosts.push(l.local_addr().unwrap());
        let n = accepted.clone();
        tokio::spawn(async move {
            let mut keep = Vec::new();
            while let Ok((s, _)) = l.accept().await {
                n.fetch_add(1, Ordering::Relaxed);
                keep.push(s); // silent: never answers, never closes
            }
        });
    }
    let seeder = node(|_| {}).await;
    let t = standard_torrent(300_000, None, false, 71);
    t.write_to(seeder.dir.path());
    seeder.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: hosts, ..AddOptions::default() }).await.unwrap();
    wait_done(&seeder, &t.id, 30).await;
    tokio::time::sleep(Duration::from_secs(6)).await;
    let dialled = accepted.load(Ordering::Relaxed);
    let totals = seeder.h.totals().await.unwrap();
    eprintln!("dialled {dialled} silent hosts in 6 s; totals {totals:?}");
    assert!((1..=4).contains(&dialled), "dialled {dialled} of 40");
    assert!(totals.half_open <= 2, "{totals:?}");
    assert_eq!(totals.connections, 0, "nothing is connected: {totals:?}");
    let snap = seeder.h.snapshot().await.unwrap();
    assert_eq!((snap.connections, snap.connecting <= 2), (0, true));

    // One real (idle) leecher: exactly one connection, and it is not "connecting".
    let mut s = TcpStream::connect(seeder.addr()).await.unwrap();
    let mut hs = vec![19u8];
    hs.extend(b"BitTorrent protocol");
    hs.extend([0u8; 8]);
    hs.extend(t.id);
    hs.extend(*b"-TR4130-oneoneoneone");
    s.write_all(&hs).await.unwrap();
    let mut theirs = [0u8; 68];
    s.read_exact(&mut theirs).await.unwrap();
    wait_for("one connection", 10, || async {
        let snap = seeder.h.snapshot().await.ok()?;
        (snap.connections == 1).then_some(())
    })
    .await;
}

/// A peer that reaches us is not refused because our own dials fill the torrent's peer slots: a dial
/// that has not connected yet gives way.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn our_pending_dials_give_way_to_a_peer_that_reached_us() {
    let mut hosts = Vec::new();
    for _ in 0..2 {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        hosts.push(l.local_addr().unwrap());
        tokio::spawn(async move {
            let mut keep = Vec::new();
            while let Ok((s, _)) = l.accept().await {
                keep.push(s); // silent: never answers, never closes
            }
        });
    }
    let seeder = node(|c| {
        c.max_connections_per_torrent = 2;
        c.warm_connections_per_torrent = 2;
        c.handshake_timeout = Duration::from_secs(30);
    })
    .await;
    let t = standard_torrent(300_000, None, false, 72);
    t.write_to(seeder.dir.path());
    seeder.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: hosts, ..AddOptions::default() }).await.unwrap();
    wait_done(&seeder, &t.id, 30).await;
    wait_for("both dials pending", 10, || async { (seeder.h.totals().await.ok()?.half_open == 2).then_some(()) }).await;
    let refused = seeder.h.diagnostics().incoming_refused;
    let mut s = TcpStream::connect(seeder.addr()).await.unwrap();
    let mut hs = vec![19u8];
    hs.extend(b"BitTorrent protocol");
    hs.extend([0u8; 8]);
    hs.extend(t.id);
    hs.extend(*b"-TR4130-herehereher1");
    s.write_all(&hs).await.unwrap();
    let mut theirs = [0u8; 68];
    s.read_exact(&mut theirs).await.unwrap();
    s.write_all(&[0, 0, 0, 1, 2]).await.unwrap(); // interested
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !matches!(read_msg(&mut s, &mut buf).await.expect("refused"), Some((1, _))) {}
    })
    .await
    .expect("not unchoked");
    assert_eq!(seeder.h.diagnostics().incoming_refused, refused);
}

/// A leecher whose connection died without a word (a router forgot it) reconnects with the same peer
/// id. It is served on the new connection, and the old, idle one goes; it was once refused until our
/// idle timeout noticed the old one, minutes later.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_peer_back_after_losing_its_connection_replaces_the_stale_one() {
    let seeder = node(|_| {}).await;
    let t = standard_torrent(300_000, None, false, 73);
    t.write_to(seeder.dir.path());
    seeder.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&seeder, &t.id, 30).await;
    async fn open(addr: std::net::SocketAddr, ih: [u8; 20]) -> TcpStream {
        let mut s = TcpStream::connect(addr).await.unwrap();
        let mut hs = vec![19u8];
        hs.extend(b"BitTorrent protocol");
        hs.extend([0u8; 8]);
        hs.extend(ih);
        hs.extend(*b"-TR4130-samesamesame");
        s.write_all(&hs).await.unwrap();
        let mut theirs = [0u8; 68];
        s.read_exact(&mut theirs).await.unwrap();
        s.write_all(&[0, 0, 0, 1, 2]).await.unwrap(); // interested
        let mut buf = Vec::new();
        tokio::time::timeout(Duration::from_secs(10), async {
            while !matches!(read_msg(&mut s, &mut buf).await.expect("hung up"), Some((1, _))) {}
        })
        .await
        .expect("not unchoked");
        s
    }
    let mut old = open(seeder.addr(), t.id).await;
    tokio::time::sleep(Duration::from_secs(11)).await;
    let _new = open(seeder.addr(), t.id).await;
    let mut buf = Vec::new();
    let closed = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if read_msg(&mut old, &mut buf).await.is_err() {
                return;
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "the stale connection stayed");
}

/// More interested leechers than upload slots: when the leechers that hold the slots finish, the one
/// left choked gets a freed slot within about a second, not at the next 10 s rechoke. The slot
/// allocator gives each of several competing torrents `total / torrents` slots, fewer than its
/// interested peers, so the leecher that arrived last stayed choked until the next rechoke: the 10 s
/// tail of about 1 in 4 first rounds of the hot benchmark (10 torrents, 3 Transmission leechers each).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_leecher_left_choked_gets_the_slot_a_finished_leecher_frees() {
    let seeder = node(|c| {
        c.upload_slots_unlimited = 4; // 2 torrents x 2 slots
        c.rechoke_interval = Duration::from_secs(4); // the idle rechokes (slots back to 0) happen before `interested`
    })
    .await;
    let mk = |name: &'static str, seed: u64| {
        make_torrent(&TorrentSpec {
            name,
            piece_len: 256 * 1024,
            files: &[(name, 48 << 20)],
            private: false,
            announce: None,
            seed,
        })
    };
    let (t1, t2) = (mk("slots1.bin", 123), mk("slots2.bin", 124));
    for t in [&t1, &t2] {
        t.write_to(seeder.dir.path());
        seeder.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
        wait_done(&seeder, &t.id, 60).await;
    }
    let addr = seeder.addr();
    // Three leechers per torrent, two slots each; the third says `interested` last and is unchoked
    // once one of the others has finished.
    let ms = Duration::from_millis;
    let (a1, b1, c1, a2, b2, c2) = tokio::join!(
        leech(addr, &t1, b'A', ms(4500), None),
        leech(addr, &t1, b'B', ms(4520), None),
        leech(addr, &t1, b'C', ms(4540), None),
        leech(addr, &t2, b'A', ms(4500), None),
        leech(addr, &t2, b'B', ms(4520), None),
        leech(addr, &t2, b'C', ms(4540), None)
    );
    for (n, l) in [("A1", &a1), ("B1", &b1), ("C1", &c1), ("A2", &a2), ("B2", &b2), ("C2", &c2)] {
        eprintln!("{n}: unchoke after {:?}", l.unchoke_after);
    }
    for l in [&c1, &c2] {
        assert!(l.unchoke_after < Duration::from_secs(2), "the third leecher waited {:?} for a slot", l.unchoke_after);
    }
}
