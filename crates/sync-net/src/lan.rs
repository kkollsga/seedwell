//! LAN discovery (SPEC §7G.3): every 30–60 s (and at start, and when a folder is added) the service sends a
//! signed UDP announcement to the local network; a device that holds one of the announced folders dials the
//! sender's sync port.
//!
//! ## The packet
//!
//! ```text
//! offset  size  field
//!  0       8    magic "SWDISC1\0"
//!  8      32    device public key (Ed25519)
//! 40       8    instance id, u64 BE (random per start, so a restart shows)
//! 48       2    sync TCP port, u16 BE
//! 50       4    Unix hour (UTC seconds / 3600), u32 BE
//! 54       8    nonce (random per announcement)
//! 62       1    tag count n (at most MAX_TAGS = 150)
//! 63      8n    tags: FolderKey::lan_tag(hour, nonce) (from the folder's public key), one per folder
//! 63+8n   64    Ed25519 signature by the device key over bytes 0 .. 63+8n
//! ```
//!
//! At most 1327 bytes. With more than 150 folders, each announcement carries the next 150 in turn.
//!
//! ## Where it goes
//!
//! UDP port [`PORT`] (27183): to the IPv4 limited broadcast 255.255.255.255, to the administratively
//! scoped IPv4 group [`GROUP_V4`] (239.192.83.87) and, when the sync listener takes IPv6, to the link-local
//! IPv6 group [`GROUP_V6`] (ff12::5357). The groups are used only when the service listens on all
//! interfaces; a service bound to one address (tests, on loopback) sends only to its extra unicast targets.
//!
//! ## What a packet costs and tells
//!
//! A receiver drops a packet that is the wrong size, has the wrong magic, comes from itself, is over the
//! rate limit ([`MAX_PER_SECOND`]), is badly signed or is more than an hour off. Then it computes, for each
//! of its folders (at most [`MAX_FOLDERS_CONSIDERED`]), the tag for the packet's hour and nonce: one HMAC per
//! folder. Only a match leads to a dial, and the dial expects the announced device key in TLS.
//!
//! An observer without a folder's key cannot tell which folders a packet names, whether two devices share
//! a folder, or link one announcement's tags to the next (the nonce is new every time). It does see the
//! device key, the port, and how many folders (up to 150) the device announces.

use std::collections::{HashMap, HashSet};
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::sync::Notify;
use trs_sync_proto::device::{self, DeviceKey};
use trs_sync_proto::ids::{DeviceId, FolderId};
use trs_sync_proto::keys::lan_tag;

use crate::service::SyncHandle;
use crate::shared::{Ctx, lock};

/// The discovery UDP port.
pub const PORT: u16 = 27183;
/// The IPv4 group (239.192.0.0/14 is the organization-local scope, RFC 2365).
pub const GROUP_V4: Ipv4Addr = Ipv4Addr::new(239, 192, 83, 87);
/// The IPv6 group (ff12: transient, link-local scope).
pub const GROUP_V6: Ipv6Addr = Ipv6Addr::new(0xff12, 0, 0, 0, 0, 0, 0, 0x5357);
/// The first 8 bytes of every packet.
pub const MAGIC: [u8; 8] = *b"SWDISC1\0";
/// Tags in one packet at most.
pub const MAX_TAGS: usize = 150;
const HEADER: usize = 63;
const SIG: usize = 64;
/// The largest packet.
pub const MAX_PACKET: usize = HEADER + 8 * MAX_TAGS + SIG;
/// Packets handled per second at most (the signature check and the HMACs); the rest are dropped.
pub const MAX_PER_SECOND: u32 = 32;
/// Folders compared against one packet at most.
pub const MAX_FOLDERS_CONSIDERED: usize = 256;

const INTERVAL: Duration = Duration::from_secs(30);
const JITTER_MS: u64 = 30_000;
/// Between two announcements at least (several folders added at once make one).
const MIN_GAP: Duration = Duration::from_millis(500);

/// What LAN discovery needs from the service.
pub(crate) struct Lan {
    pub(crate) ctx: Arc<Ctx>,
    /// The sync listener's address: all interfaces means broadcast and multicast; one address means only
    /// `targets`, from a socket bound to it.
    pub(crate) bind: IpAddr,
    /// The discovery port.
    pub(crate) port: u16,
    /// Extra unicast destinations of every announcement.
    pub(crate) targets: Vec<SocketAddr>,
    /// A folder was added: announce now.
    pub(crate) wake: Notify,
}

impl Lan {
    pub(crate) fn new(ctx: Arc<Ctx>, bind: IpAddr, port: u16, targets: Vec<SocketAddr>) -> Lan {
        Lan { ctx, bind, port, targets, wake: Notify::new() }
    }
}

/// One announcement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Announcement {
    /// The sender's device public key.
    pub device_pk: [u8; 32],
    /// Random per start of the sender.
    pub instance: u64,
    /// The sender's sync TCP port.
    pub port: u16,
    /// Unix hour the tags are for.
    pub hour: u32,
    /// Random per announcement.
    pub nonce: [u8; 8],
    /// One per folder (at most [`MAX_TAGS`]; more are left out).
    pub tags: Vec<[u8; 8]>,
}

impl Announcement {
    /// The packet, signed with `key` (it verifies only if `key` is the key of `device_pk`).
    pub fn encode(&self, key: &DeviceKey) -> Vec<u8> {
        let tags = &self.tags[..self.tags.len().min(MAX_TAGS)];
        let mut b = Vec::with_capacity(HEADER + 8 * tags.len() + SIG);
        b.extend_from_slice(&MAGIC);
        b.extend_from_slice(&self.device_pk);
        b.extend_from_slice(&self.instance.to_be_bytes());
        b.extend_from_slice(&self.port.to_be_bytes());
        b.extend_from_slice(&self.hour.to_be_bytes());
        b.extend_from_slice(&self.nonce);
        b.push(tags.len() as u8);
        for t in tags {
            b.extend_from_slice(t);
        }
        let sig = key.sign(&b);
        b.extend_from_slice(&sig);
        b
    }

    /// A packet's announcement, if it is well formed and signed by the key it names.
    pub fn decode(b: &[u8]) -> Option<Announcement> {
        let (a, signed, sig) = parse(b)?;
        device::verify(&a.device_pk, signed, &sig).then_some(a)
    }
}

fn arr<const N: usize>(b: &[u8]) -> [u8; N] {
    let mut out = [0u8; N];
    out.copy_from_slice(&b[..N]);
    out
}

/// The fields, the signed bytes and the signature, without checking the signature.
fn parse(b: &[u8]) -> Option<(Announcement, &[u8], [u8; 64])> {
    if b.len() < HEADER + SIG || b.len() > MAX_PACKET || b[..8] != MAGIC {
        return None;
    }
    let n = b[62] as usize;
    if n > MAX_TAGS || b.len() != HEADER + 8 * n + SIG {
        return None;
    }
    let tags = b[HEADER..HEADER + 8 * n].as_chunks::<8>().0.to_vec();
    let a = Announcement {
        device_pk: arr(&b[8..40]),
        instance: u64::from_be_bytes(arr(&b[40..48])),
        port: u16::from_be_bytes(arr(&b[48..50])),
        hour: u32::from_be_bytes(arr(&b[50..54])),
        nonce: arr(&b[54..62]),
        tags,
    };
    let end = HEADER + 8 * n;
    Some((a, &b[..end], arr(&b[end..])))
}

/// Which of `n` folders announcement number `round` carries: all when they fit, else the next `max` in
/// turn, so every folder is announced at least once every `ceil(n / max)` rounds.
pub fn rotation(n: usize, round: u64, max: usize) -> Vec<usize> {
    if n <= max {
        return (0..n).collect();
    }
    let start = ((round % n as u64) as usize * max) % n;
    (0..max).map(|i| (start + i) % n).collect()
}

/// The current Unix hour.
pub fn unix_hour() -> u32 {
    (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()) / 3600) as u32
}

fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    let _ = ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut b);
    b
}

/// Runs LAN discovery for the service until it stops (the task is aborted).
pub async fn run(h: SyncHandle) {
    let lan = h.lan();
    let groups = lan.bind.is_unspecified();
    // An IPv4 socket unless the listener is on one IPv6 address; an IPv6 one only when the listener takes
    // IPv6 (the default `0.0.0.0` does not: a peer could not dial the address it would learn).
    let v4_ip = match lan.bind {
        IpAddr::V4(ip) => Some(ip),
        IpAddr::V6(ip) if ip.is_unspecified() => Some(Ipv4Addr::UNSPECIFIED),
        IpAddr::V6(_) => None,
    };
    let v6_ip = match lan.bind {
        IpAddr::V6(ip) => Some(ip),
        IpAddr::V4(_) => None,
    };
    let v4 = v4_ip.and_then(|ip| match open(SocketAddr::new(ip.into(), lan.port), groups) {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("seedwell sync: LAN discovery: cannot use UDP port {}: {e}", lan.port);
            None
        }
    });
    let v6 = v6_ip.and_then(|ip| open(SocketAddr::new(ip.into(), lan.port), groups).ok());
    if v4.is_none() && v6.is_none() {
        return;
    }
    let instance = u64::from_be_bytes(random());
    let rx = Mutex::new(Rx::new());
    tokio::join!(
        send_loop(&h, v4.as_ref(), v6.as_ref(), groups, instance),
        recv_loop(&h, v4.as_ref(), &rx),
        recv_loop(&h, v6.as_ref(), &rx),
    );
}

/// A UDP socket on the discovery port, shared with other processes on this machine (SO_REUSEADDR and
/// SO_REUSEPORT: the app and a daemon, or tests, can all listen; broadcast and multicast reach each).
fn open(addr: SocketAddr, groups: bool) -> io::Result<UdpSocket> {
    let s = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))?;
    s.set_reuse_address(true)?;
    s.set_reuse_port(true)?;
    if addr.is_ipv6() {
        s.set_only_v6(true)?;
    }
    s.bind(&addr.into())?;
    s.set_nonblocking(true)?;
    let s = UdpSocket::from_std(s.into())?;
    if groups {
        if addr.is_ipv4() {
            s.set_broadcast(true)?;
            s.set_multicast_ttl_v4(1)?;
            s.set_multicast_loop_v4(true)?;
        } else {
            s.set_multicast_loop_v6(true)?;
        }
    }
    Ok(s)
}

/// Remembers which destinations are failing, so each failure is logged once until it works again (no Local
/// Network permission, no network: EHOSTUNREACH, EPERM, ENETUNREACH; it is retried at the next round).
#[derive(Default)]
struct Quiet {
    failing: HashSet<SocketAddr>,
}

impl Quiet {
    fn note<T>(&mut self, what: &str, dest: SocketAddr, r: io::Result<T>) {
        match r {
            Ok(_) => {
                self.failing.remove(&dest);
            }
            Err(e) => {
                if self.failing.insert(dest) {
                    eprintln!("seedwell sync: LAN discovery: {what} {dest} failed: {e} (retrying quietly)");
                }
            }
        }
    }
}

async fn send_loop(h: &SyncHandle, v4: Option<&UdpSocket>, v6: Option<&UdpSocket>, groups: bool, instance: u64) {
    let lan = h.lan();
    let mut quiet = Quiet::default();
    let mut round = 0u64;
    let g4 = SocketAddr::V4(SocketAddrV4::new(GROUP_V4, lan.port));
    let g6 = SocketAddr::V6(SocketAddrV6::new(GROUP_V6, lan.port, 0, 0));
    let mut dests = Vec::new();
    if groups {
        dests.push(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::BROADCAST, lan.port)));
        dests.push(g4);
        dests.push(g6);
    }
    dests.extend(lan.targets.iter().copied());
    loop {
        if groups {
            // Joined again every round: after a network change the default interface is another one. An
            // existing membership answers EADDRINUSE, which is fine.
            if let Some(s) = v4 {
                let r = s.join_multicast_v4(GROUP_V4, Ipv4Addr::UNSPECIFIED);
                quiet.note("joining", g4, r.or_else(|e| if e.kind() == io::ErrorKind::AddrInUse { Ok(()) } else { Err(e) }));
            }
            if let Some(s) = v6 {
                let r = s.join_multicast_v6(&GROUP_V6, 0);
                quiet.note("joining", g6, r.or_else(|e| if e.kind() == io::ErrorKind::AddrInUse { Ok(()) } else { Err(e) }));
            }
        }
        if let Some(pkt) = build(h, instance, round) {
            round = round.wrapping_add(1);
            for &dest in &dests {
                let Some(s) = (if dest.is_ipv4() { v4 } else { v6 }) else { continue };
                let r = s.send_to(&pkt, dest).await;
                quiet.note("sending to", dest, r);
            }
        }
        let wait = INTERVAL + Duration::from_millis(u64::from_be_bytes(random()) % JITTER_MS);
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = lan.wake.notified() => {}
        }
        tokio::time::sleep(MIN_GAP).await;
    }
}

/// This round's packet, or nothing (LAN discovery switched off, or no folder to announce).
fn build(h: &SyncHandle, instance: u64, round: u64) -> Option<Vec<u8>> {
    if !h.settings().lan_discovery {
        return None;
    }
    let ctx = &h.lan().ctx;
    let folders: Vec<_> = ctx.registry.spaces().into_iter().filter(|f| !f.paused.load(Ordering::Relaxed)).collect();
    if folders.is_empty() {
        return None;
    }
    let hour = unix_hour();
    let nonce = random();
    let tags = rotation(folders.len(), round, MAX_TAGS).into_iter().map(|i| folders[i].public.lan_tag(hour, &nonce)).collect();
    let a = Announcement { device_pk: *ctx.key.public_key(), instance, port: h.port(), hour, nonce, tags };
    Some(a.encode(&ctx.key))
}

/// The receivers' state (both sockets share it).
struct Rx {
    window: Instant,
    count: u32,
    /// The instance id last seen per device: a new one means it started again.
    instances: HashMap<DeviceId, u64>,
    lan_keys: HashMap<FolderId, [u8; 32]>,
}

impl Rx {
    fn new() -> Rx {
        Rx { window: Instant::now(), count: 0, instances: HashMap::new(), lan_keys: HashMap::new() }
    }

    /// May one more packet be handled now?
    fn admit(&mut self, now: Instant) -> bool {
        if now.duration_since(self.window) >= Duration::from_secs(1) {
            self.window = now;
            self.count = 0;
        }
        self.count += 1;
        self.count <= MAX_PER_SECOND
    }
}

async fn recv_loop(h: &SyncHandle, sock: Option<&UdpSocket>, rx: &Mutex<Rx>) {
    let Some(sock) = sock else { return };
    // One byte more than the largest packet: a longer datagram is truncated to it and refused.
    let mut buf = vec![0u8; MAX_PACKET + 1];
    let mut backoff = Duration::from_millis(100);
    loop {
        match sock.recv_from(&mut buf).await {
            Ok((n, src)) => {
                backoff = Duration::from_millis(100);
                handle(h, rx, &buf[..n], src);
            }
            Err(_) => {
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(5));
            }
        }
    }
}

/// One received datagram.
fn handle(h: &SyncHandle, rx: &Mutex<Rx>, b: &[u8], src: SocketAddr) {
    let Some((a, signed, sig)) = parse(b) else { return };
    let ctx = &h.lan().ctx;
    if a.device_pk == *ctx.key.public_key() || a.port == 0 {
        return;
    }
    if !lock(rx).admit(Instant::now()) {
        return;
    }
    if !device::verify(&a.device_pk, signed, &sig) || unix_hour().abs_diff(a.hour) > 1 {
        return;
    }
    if !h.settings().lan_discovery {
        return;
    }
    let device = DeviceId::of_public_key(&a.device_pk);
    // The source address with the announced port (an IPv6 source keeps its scope id: link-local needs it).
    let mut to = src;
    to.set_port(a.port);
    let mut found = Vec::new();
    {
        let mut rx = lock(rx);
        if rx.lan_keys.len() > 4 * MAX_FOLDERS_CONSIDERED {
            rx.lan_keys.clear();
        }
        for f in ctx.registry.spaces().iter().filter(|f| !f.paused.load(Ordering::Relaxed)).take(MAX_FOLDERS_CONSIDERED) {
            let k = *rx.lan_keys.entry(f.id).or_insert_with(|| f.public.lan_key());
            if a.tags.contains(&lan_tag(&k, a.hour, &a.nonce)) {
                found.push(f.id);
            }
        }
        if found.is_empty() {
            return;
        }
        if rx.instances.len() > 1024 {
            rx.instances.clear();
        }
        if rx.instances.insert(device, a.instance) != Some(a.instance) {
            // Seen for the first time, or it started again: dial now, whatever the backoff says.
            h.retry_now(to);
        }
    }
    for f in found {
        h.learn_address(Some(device), Some(f), to);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trs_sync_proto::keys::{FolderKey, FolderSecret};

    fn key(seed: u8) -> DeviceKey {
        DeviceKey::from_seed(&[seed; 32])
    }

    fn sample(k: &DeviceKey, tags: usize) -> Announcement {
        Announcement {
            device_pk: *k.public_key(),
            instance: 0x0102_0304_0506_0708,
            port: 22_000,
            hour: 491_234,
            nonce: [9; 8],
            tags: (0..tags).map(|i| [i as u8; 8]).collect(),
        }
    }

    #[test]
    fn encode_decode_round_trip() {
        let k = key(1);
        for n in [0, 1, 3, MAX_TAGS] {
            let a = sample(&k, n);
            let b = a.encode(&k);
            assert_eq!(b.len(), HEADER + 8 * n + SIG);
            assert_eq!(&b[..8], b"SWDISC1\0");
            assert_eq!(Announcement::decode(&b), Some(a));
        }
    }

    #[test]
    fn the_largest_packet_fits_and_extra_tags_are_left_out() {
        let k = key(1);
        let b = sample(&k, MAX_TAGS + 40).encode(&k);
        assert_eq!(b.len(), MAX_PACKET);
        const { assert!(MAX_PACKET <= 1400) };
        assert_eq!(Announcement::decode(&b).map(|a| a.tags.len()), Some(MAX_TAGS));
    }

    #[test]
    fn any_change_breaks_the_signature() {
        let k = key(1);
        let b = sample(&k, 2).encode(&k);
        for i in 8..b.len() {
            let mut c = b.clone();
            c[i] ^= 0x01;
            assert_eq!(Announcement::decode(&c), None, "byte {i} flipped");
        }
        // Signed by another key than the one it names.
        assert_eq!(Announcement::decode(&sample(&k, 2).encode(&key(2))), None);
    }

    #[test]
    fn malformed_packets_are_refused() {
        let k = key(1);
        let b = sample(&k, 2).encode(&k);
        assert_eq!(Announcement::decode(&b[..b.len() - 1]), None, "truncated");
        let mut long = b.clone();
        long.push(0);
        assert_eq!(Announcement::decode(&long), None, "trailing byte");
        let mut magic = b.clone();
        magic[6] = b'2';
        assert_eq!(Announcement::decode(&magic), None, "magic");
        let mut count = b.clone();
        count[62] = 3;
        assert_eq!(Announcement::decode(&count), None, "count does not match the length");
        assert_eq!(Announcement::decode(&[0u8; MAX_PACKET + 1]), None, "oversized");
        assert_eq!(Announcement::decode(&[]), None);
    }

    #[test]
    fn rotation_covers_every_folder() {
        assert_eq!(rotation(3, 7, MAX_TAGS), vec![0, 1, 2]);
        assert!(rotation(0, 0, MAX_TAGS).is_empty());
        for n in [151usize, 300, 400, 1000] {
            let rounds = n.div_ceil(MAX_TAGS) as u64;
            let mut seen = HashSet::new();
            for r in 0..rounds {
                let pick = rotation(n, r, MAX_TAGS);
                assert_eq!(pick.len(), MAX_TAGS);
                assert_eq!(pick.iter().collect::<HashSet<_>>().len(), MAX_TAGS, "no folder twice in one packet");
                seen.extend(pick);
            }
            assert_eq!(seen.len(), n, "{n} folders in {rounds} rounds");
        }
        // It keeps going round (any window of rounds covers everything).
        let mut seen = HashSet::new();
        for r in 1000..1003 {
            seen.extend(rotation(400, r, MAX_TAGS));
        }
        assert_eq!(seen.len(), 400);
    }

    #[test]
    fn tags_match_only_the_same_folder_and_change_with_the_nonce() {
        let s = FolderSecret::generate(&mut ring::rand::SystemRandom::new()).unwrap();
        let ro = FolderKey::Member(*s.public_key());
        let rw = FolderKey::Owner(s);
        let other = FolderKey::Owner(FolderSecret::generate(&mut ring::rand::SystemRandom::new()).unwrap());
        let n1 = [1u8; 8];
        assert_eq!(rw.lan_tag(10, &n1), ro.lan_tag(10, &n1), "owners and members compute the same tag");
        assert_eq!(lan_tag(&ro.lan_key(), 10, &n1), rw.lan_tag(10, &n1));
        assert_ne!(rw.lan_tag(10, &n1), other.lan_tag(10, &n1));
        assert_ne!(rw.lan_tag(10, &n1), rw.lan_tag(10, &[2u8; 8]), "unlinkable across announcements");
        assert_ne!(rw.lan_tag(10, &n1), rw.lan_tag(11, &n1));
    }

    #[test]
    fn the_rate_limit_drops_the_rest_of_a_second() {
        let mut rx = Rx::new();
        let t = Instant::now();
        let admitted = (0..100).filter(|_| rx.admit(t)).count();
        assert_eq!(admitted, MAX_PER_SECOND as usize);
        assert!(rx.admit(t + Duration::from_secs(1)), "a new second");
    }
}
