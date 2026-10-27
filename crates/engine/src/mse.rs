//! Message Stream Encryption: the obfuscated handshake that Transmission, libtorrent, uTorrent and
//! most other clients open connections with (and the reason a client without it fails every first
//! contact with them: the first bytes are not "BitTorrent protocol").
//!
//! * **Responder** ([`respond`]): an incoming connection whose first bytes are not a plain
//!   handshake. After the Diffie-Hellman exchange the initiator proves it knows the torrent
//!   (`SKEY`, the info-hash), says which ciphers it offers (`crypto_provide`: 1 = plain after the
//!   handshake, 2 = RC4) and sends its BitTorrent handshake as the initial payload. We pick plain
//!   when it is offered and the policy allows, else RC4.
//! * **Initiator** ([`initiate`]): the same exchange from our side, with our handshake as the
//!   initial payload.
//!
//! The primitives (group arithmetic, RC4) are in `trs_wire::mse`. This module is the byte shuffling
//! on a socket: every length is bounded, every wait is bounded by the caller's deadline, and a
//! malformed message is an error, never a panic.

use std::io;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use trs_wire::mse::{self, KEY_LEN, Rc4, SECRET_LEN};
use trs_wire::{HANDSHAKE_LEN, Handshake};

use crate::config::EncryptionPolicy;
use crate::net::{PeerIo, Rc4Io};

/// Longest random padding in the specification (`PadA`, `PadB`, `PadC`, `PadD`).
const MAX_PAD: usize = 512;
const CRYPTO_PLAIN: u32 = 1;
const CRYPTO_RC4: u32 = 2;

fn bad(why: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, why)
}

fn random(n: usize) -> io::Result<Vec<u8>> {
    let mut v = vec![0u8; n];
    getrandom::fill(&mut v).map_err(|_| io::Error::other("no randomness"))?;
    Ok(v)
}

fn random_secret() -> io::Result<[u8; SECRET_LEN]> {
    let mut x = [0u8; SECRET_LEN];
    getrandom::fill(&mut x).map_err(|_| io::Error::other("no randomness"))?;
    Ok(x)
}

fn pad_len() -> io::Result<usize> {
    let mut b = [0u8; 2];
    getrandom::fill(&mut b).map_err(|_| io::Error::other("no randomness"))?;
    // Short paddings are enough to hide the lengths; they keep the first flight small.
    Ok(usize::from(u16::from_le_bytes(b)) % 129)
}

fn xor20(a: &[u8; 20], b: &[u8; 20]) -> [u8; 20] {
    let mut o = [0u8; 20];
    for i in 0..20 {
        o[i] = a[i] ^ b[i];
    }
    o
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Read until `buf` holds at least `want` bytes.
async fn fill(s: &mut PeerIo, buf: &mut Vec<u8>, want: usize) -> io::Result<()> {
    let mut tmp = [0u8; 1024];
    while buf.len() < want {
        let room = (want - buf.len()).min(tmp.len());
        let n = s.read(&mut tmp[..room]).await?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    Ok(())
}

/// What the responder found out.
pub(crate) struct Accepted {
    /// The connection, plain or RC4, positioned after the initiator's handshake.
    pub io: PeerIo,
    /// The initiator's BitTorrent handshake (its initial payload).
    pub handshake: Handshake,
}

/// Answer an encrypted handshake. `first` are the bytes already read from the connection (at
/// least 20, they started the Diffie-Hellman value); `find_torrent` maps `H("req2", info-hash)` to
/// a torrent we have.
pub(crate) async fn respond<F, Fut>(
    mut s: PeerIo,
    first: &[u8],
    policy: EncryptionPolicy,
    cpu: &tokio::sync::Semaphore,
    find_torrent: F,
) -> io::Result<Accepted>
where
    F: FnOnce([u8; 20]) -> Fut,
    Fut: std::future::Future<Output = Option<[u8; 20]>>,
{
    if policy == EncryptionPolicy::Off {
        return Err(bad("encryption is off"));
    }
    // 1. Ya
    let mut buf = first.to_vec();
    fill(&mut s, &mut buf, KEY_LEN).await?;
    let mut ya = [0u8; KEY_LEN];
    ya.copy_from_slice(&buf[..KEY_LEN]);
    // Two modular exponentiations (about a millisecond in a release build): `cpu` bounds the concurrent
    // ones, so that garbage sent to the listen port cannot burn the CPU. Only the arithmetic holds a
    // turn, never a round trip to the peer: a slow or silent peer must not hold up the others. Thirty
    // leecher connections arrive in the same instant when a client with ten torrents starts, and each
    // waits for a turn rather than being refused (a refused one is retried only after a back-off).
    let (secret, yb) = {
        let _turn = cpu.acquire().await.map_err(|_| bad("shutting down"))?;
        let x = random_secret()?;
        let secret = mse::shared_secret(&x, &ya).ok_or_else(|| bad("invalid public value"))?;
        (secret, mse::public_key(&x))
    };
    // 2. Yb + PadB
    let mut out = yb.to_vec();
    out.extend(random(pad_len()?)?);
    s.write_all(&out).await?;
    // 3. PadA, H("req1", S), H("req2", SKEY) ^ H("req3", S)
    let req1 = mse::hash(&[b"req1", &secret]);
    let mut rest: Vec<u8> = buf.split_off(KEY_LEN);
    let at = loop {
        if let Some(i) = find(&rest, &req1) {
            break i;
        }
        if rest.len() > MAX_PAD + 20 {
            return Err(bad("no req1 marker"));
        }
        let want = rest.len() + 1;
        fill(&mut s, &mut rest, want).await?;
    };
    fill(&mut s, &mut rest, at + 40).await?;
    let mut masked = [0u8; 20];
    masked.copy_from_slice(&rest[at + 20..at + 40]);
    let req2 = xor20(&masked, &mse::hash(&[b"req3", &secret]));
    let skey = find_torrent(req2).await.ok_or_else(|| bad("unknown torrent"))?;
    // 4. ENCRYPT(VC, crypto_provide, len(PadC), PadC, len(IA)), ENCRYPT(IA)
    let mut dec = Rc4::new(&mse::hash(&[b"keyA", &secret, &skey]));
    let mut enc = Rc4::new(&mse::hash(&[b"keyB", &secret, &skey]));
    let head = at + 40;
    fill(&mut s, &mut rest, head + 14).await?;
    let mut hdr = [0u8; 14];
    hdr.copy_from_slice(&rest[head..head + 14]);
    dec.apply(&mut hdr);
    if hdr[..8].iter().any(|b| *b != 0) {
        return Err(bad("bad verification constant"));
    }
    let provide = u32::from_be_bytes([hdr[8], hdr[9], hdr[10], hdr[11]]);
    let pad_c = usize::from(u16::from_be_bytes([hdr[12], hdr[13]]));
    if pad_c > MAX_PAD {
        return Err(bad("PadC too long"));
    }
    fill(&mut s, &mut rest, head + 14 + pad_c + 2).await?;
    let mut tail = rest[head + 14..head + 14 + pad_c + 2].to_vec();
    dec.apply(&mut tail);
    let ia_len = usize::from(u16::from_be_bytes([tail[pad_c], tail[pad_c + 1]]));
    // The initial payload is the initiator's handshake, nothing else (it waits for our answer).
    if ia_len > HANDSHAKE_LEN {
        return Err(bad("initial payload is not a handshake"));
    }
    let end = head + 14 + pad_c + 2 + ia_len;
    fill(&mut s, &mut rest, end).await?;
    if rest.len() > end {
        return Err(bad("data after the initial payload"));
    }
    let mut ia = rest[end - ia_len..end].to_vec();
    dec.apply(&mut ia);
    // 5. Choose. Plain is cheaper; RC4 when it is all that is offered or the policy insists.
    let select = if provide & CRYPTO_PLAIN != 0 && policy == EncryptionPolicy::Prefer {
        CRYPTO_PLAIN
    } else if provide & CRYPTO_RC4 != 0 {
        CRYPTO_RC4
    } else {
        return Err(bad("no acceptable cipher offered"));
    };
    // ENCRYPT(VC, crypto_select, len(PadD)) with an empty PadD
    let mut reply = [0u8; 14];
    reply[8..12].copy_from_slice(&select.to_be_bytes());
    enc.apply(&mut reply);
    s.write_all(&reply).await?;
    // The rest of the initiator's handshake (if its initial payload held only part) follows in the
    // chosen cipher.
    let mut io = if select == CRYPTO_RC4 {
        PeerIo::Rc4(Box::new(Rc4Io::new(s, dec, enc)))
    } else {
        s
    };
    let mut hs = [0u8; HANDSHAKE_LEN];
    hs[..ia.len()].copy_from_slice(&ia);
    if ia.len() < HANDSHAKE_LEN {
        io.read_exact(&mut hs[ia.len()..]).await?;
    }
    let handshake = match Handshake::decode(&hs) {
        Ok(Some(h)) => h,
        _ => return Err(bad("bad handshake")),
    };
    if handshake.info_hash != skey {
        return Err(bad("handshake names another torrent"));
    }
    Ok(Accepted { io, handshake })
}

/// Open an encrypted connection from our side. `ours` is our BitTorrent handshake (sent as the
/// initial payload); `skey` the info-hash. On success the connection is positioned right before the
/// peer's handshake. With `policy == Require` only RC4 is offered.
pub(crate) async fn initiate(mut s: PeerIo, skey: &[u8; 20], ours: &[u8], policy: EncryptionPolicy) -> io::Result<PeerIo> {
    if policy == EncryptionPolicy::Off {
        return Err(bad("encryption is off"));
    }
    // 1. Ya + PadA
    let x = random_secret()?;
    let mut out = mse::public_key(&x).to_vec();
    out.extend(random(pad_len()?)?);
    s.write_all(&out).await?;
    // 2. Yb
    let mut buf = Vec::new();
    fill(&mut s, &mut buf, KEY_LEN).await?;
    let mut yb = [0u8; KEY_LEN];
    yb.copy_from_slice(&buf[..KEY_LEN]);
    let secret = mse::shared_secret(&x, &yb).ok_or_else(|| bad("invalid public value"))?;
    // 3. H("req1", S), H("req2", SKEY) ^ H("req3", S), ENCRYPT(VC, provide, len(PadC), PadC, len(IA)), ENCRYPT(IA)
    let mut enc = Rc4::new(&mse::hash(&[b"keyA", &secret, skey]));
    let mut dec = Rc4::new(&mse::hash(&[b"keyB", &secret, skey]));
    let provide = if policy == EncryptionPolicy::Require { CRYPTO_RC4 } else { CRYPTO_PLAIN | CRYPTO_RC4 };
    let pad_c = random(pad_len()? % 33)?;
    let mut body = vec![0u8; 8];
    body.extend(provide.to_be_bytes());
    body.extend((pad_c.len() as u16).to_be_bytes());
    body.extend(&pad_c);
    body.extend((ours.len() as u16).to_be_bytes());
    body.extend(ours);
    enc.apply(&mut body);
    let mut flight = mse::hash(&[b"req1", &secret]).to_vec();
    flight.extend(xor20(&mse::hash(&[b"req2", skey]), &mse::hash(&[b"req3", &secret])));
    flight.extend(body);
    s.write_all(&flight).await?;
    // 4. PadB, then ENCRYPT(VC, crypto_select, len(PadD)), PadD: find the encrypted VC (the first
    //    eight bytes of the responder's key stream).
    let mut vc = [0u8; 8];
    dec.clone().apply(&mut vc);
    let mut rest: Vec<u8> = buf.split_off(KEY_LEN);
    let at = loop {
        if let Some(i) = find(&rest, &vc) {
            break i;
        }
        if rest.len() > MAX_PAD + 8 {
            return Err(bad("no verification constant"));
        }
        let want = rest.len() + 1;
        fill(&mut s, &mut rest, want).await?;
    };
    fill(&mut s, &mut rest, at + 14).await?;
    let mut hdr = [0u8; 14];
    hdr.copy_from_slice(&rest[at..at + 14]);
    dec.apply(&mut hdr);
    if hdr[..8].iter().any(|b| *b != 0) {
        return Err(bad("bad verification constant"));
    }
    let select = u32::from_be_bytes([hdr[8], hdr[9], hdr[10], hdr[11]]);
    let pad_d = usize::from(u16::from_be_bytes([hdr[12], hdr[13]]));
    if pad_d > MAX_PAD || select & !provide != 0 || (select != CRYPTO_PLAIN && select != CRYPTO_RC4) {
        return Err(bad("unacceptable crypto_select"));
    }
    fill(&mut s, &mut rest, at + 14 + pad_d).await?;
    let mut padd = rest[at + 14..at + 14 + pad_d].to_vec();
    dec.apply(&mut padd);
    // What follows PadD is the peer's handshake. Anything already read belongs to it, and the
    // peer sends nothing before it has our payload, so there is at most that much; it must be
    // pushed back, which a plain socket cannot do: only accept the common case of nothing extra.
    if rest.len() > at + 14 + pad_d {
        return Err(bad("data after the encryption handshake"));
    }
    Ok(if select == CRYPTO_RC4 { PeerIo::Rc4(Box::new(Rc4Io::new(s, dec, enc))) } else { s })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::{TcpListener, TcpStream};

    fn handshake(ih: [u8; 20], tag: u8) -> Vec<u8> {
        let mut pid = [b'-'; 20];
        pid[1] = tag;
        Handshake { reserved: trs_wire::Reserved::ours(false), info_hash: ih, peer_id: pid }.to_bytes().to_vec()
    }

    async fn pair() -> (PeerIo, PeerIo) {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (c, a) = tokio::join!(TcpStream::connect(l.local_addr().unwrap()), l.accept());
        (PeerIo::Tcp(c.unwrap()), PeerIo::Tcp(a.unwrap().0))
    }

    async fn run(initiator: EncryptionPolicy, responder: EncryptionPolicy) -> (io::Result<PeerIo>, io::Result<Accepted>) {
        let ih = [0x42u8; 20];
        let (c, mut s) = pair().await;
        let ours = handshake(ih, b'I');
        let init = tokio::spawn(async move { initiate(c, &ih, &ours, initiator).await });
        let mut first = [0u8; 20];
        s.read_exact(&mut first).await.unwrap();
        let acc = respond(s, &first, responder, &tokio::sync::Semaphore::new(1), |req2| async move {
            (req2 == mse::hash(&[b"req2", &ih])).then_some(ih)
        })
        .await;
        (init.await.unwrap(), acc)
    }

    async fn talk(mut a: PeerIo, mut b: PeerIo) {
        // Data both ways, big enough to cross the 64 KiB write chunk of the cipher layer.
        let payload: Vec<u8> = (0..300_000u32).map(|i| (i * 7) as u8).collect();
        let p2 = payload.clone();
        let w = tokio::spawn(async move {
            a.write_all(&p2).await.unwrap();
            let mut back = vec![0u8; 5];
            a.read_exact(&mut back).await.unwrap();
            assert_eq!(&back, b"pong!");
        });
        let mut got = vec![0u8; payload.len()];
        b.read_exact(&mut got).await.unwrap();
        assert!(got == payload);
        b.write_all(b"pong!").await.unwrap();
        w.await.unwrap();
    }

    #[tokio::test]
    async fn prefer_meets_prefer_and_stays_plain_after_the_handshake() {
        let (i, r) = run(EncryptionPolicy::Prefer, EncryptionPolicy::Prefer).await;
        let (i, r) = (i.unwrap(), r.unwrap());
        assert!(!i.is_encrypted() && !r.io.is_encrypted(), "plain is chosen when offered");
        assert_eq!(r.handshake.info_hash, [0x42; 20]);
        talk(i, r.io).await;
    }

    #[tokio::test]
    async fn require_uses_rc4_on_both_ends_and_carries_data() {
        let (i, r) = run(EncryptionPolicy::Require, EncryptionPolicy::Prefer).await;
        let (i, r) = (i.unwrap(), r.unwrap());
        assert!(i.is_encrypted() && r.io.is_encrypted());
        talk(i, r.io).await;
        let (i, r) = run(EncryptionPolicy::Prefer, EncryptionPolicy::Require).await;
        let (i, r) = (i.unwrap(), r.unwrap());
        assert!(i.is_encrypted() && r.io.is_encrypted(), "a responder that requires RC4 picks it");
        talk(i, r.io).await;
    }

    #[tokio::test]
    async fn an_unknown_torrent_and_a_disabled_policy_are_refused() {
        let (_, r) = run(EncryptionPolicy::Prefer, EncryptionPolicy::Off).await;
        assert!(r.is_err());
        let ih = [1u8; 20];
        let (c, mut s) = pair().await;
        let init = tokio::spawn(async move { initiate(c, &ih, &handshake(ih, b'I'), EncryptionPolicy::Prefer).await });
        let mut first = [0u8; 20];
        s.read_exact(&mut first).await.unwrap();
        let r = respond(s, &first, EncryptionPolicy::Prefer, &tokio::sync::Semaphore::new(1), |_| async { None }).await;
        assert!(r.is_err());
        assert!(init.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn a_silent_peer_does_not_hold_up_the_next_encrypted_handshake() {
        // One turn for the arithmetic. Peer A sends its key and then nothing: it must not keep the turn
        // while we wait for the rest of its handshake.
        let cpu = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        let (mut a, mut sa) = pair().await;
        let x = random_secret().unwrap();
        a.write_all(&mse::public_key(&x)).await.unwrap();
        let mut first = [0u8; 20];
        sa.read_exact(&mut first).await.unwrap();
        let cpu2 = cpu.clone();
        let stalled = tokio::spawn(async move { respond(sa, &first, EncryptionPolicy::Prefer, &cpu2, |_| async { None }).await });
        let mut yb = [0u8; KEY_LEN];
        a.read_exact(&mut yb).await.unwrap();
        // A's answer came, so its arithmetic is done; B goes through within a moment.
        let ih = [7u8; 20];
        let (c, mut s) = pair().await;
        let init = tokio::spawn(async move { initiate(c, &ih, &handshake(ih, b'B'), EncryptionPolicy::Prefer).await });
        let mut first = [0u8; 20];
        s.read_exact(&mut first).await.unwrap();
        let acc = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            respond(s, &first, EncryptionPolicy::Prefer, &cpu, |req2| async move { (req2 == mse::hash(&[b"req2", &ih])).then_some(ih) }),
        )
        .await
        .expect("B waited for A's silent handshake");
        assert_eq!(acc.unwrap().handshake.info_hash, ih);
        assert!(init.await.unwrap().is_ok());
        assert!(!stalled.is_finished(), "A is still waiting for its peer");
        stalled.abort();
        drop(a);
    }

    #[tokio::test]
    async fn garbage_never_panics() {
        for len in [20usize, 96, 300, 700] {
            let (mut c, s) = pair().await;
            let junk: Vec<u8> = (0..len).map(|i| (i * 31 + 7) as u8).collect();
            let first = junk[..20].to_vec();
            let w = tokio::spawn(async move {
                let _ = c.write_all(&junk).await;
                let _ = c.shutdown().await;
            });
            let r = respond(s, &first, EncryptionPolicy::Prefer, &tokio::sync::Semaphore::new(1), |_| async { None }).await;
            assert!(r.is_err());
            let _ = w.await;
        }
    }
}
