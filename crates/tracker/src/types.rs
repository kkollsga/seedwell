//! Plain data types shared by the core and the transports.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// Info-hash bytes as sent to trackers (v2 torrents use the truncated hash).
pub type InfoHash20 = [u8; 20];
/// Peer-id bytes.
pub type PeerId20 = [u8; 20];

/// Identifies one logical request of one announce machine. Unique per machine
/// for its whole life; responses are matched on it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct RequestId(pub u64);

/// BEP 3 announce `event`. A regular announce has no event (`Option::None`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Event {
    /// First announce of a session.
    Started,
    /// The download finished (at most once per torrent per tracker, ever).
    Completed,
    /// The session ends (pause, remove, quit).
    Stopped,
}

impl Event {
    /// Wire spelling for HTTP.
    pub const fn as_str(self) -> &'static str {
        match self {
            Event::Started => "started",
            Event::Completed => "completed",
            Event::Stopped => "stopped",
        }
    }
    /// BEP 15 event code.
    pub const fn udp_code(self) -> u32 {
        match self {
            Event::Completed => 1,
            Event::Started => 2,
            Event::Stopped => 3,
        }
    }
}

/// Payload byte counters as the engine knows them right now.
///
/// `uploaded`/`downloaded` are payload bytes of the *current session*;
/// `left` is exact; `size` is the torrent's total length. The machines
/// sanitise these (see invariant 4), so a sloppy caller cannot make us report
/// decreasing counters or `left > size`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Stats {
    /// Payload bytes uploaded this session.
    pub uploaded: u64,
    /// Payload bytes downloaded this session.
    pub downloaded: u64,
    /// Bytes still missing.
    pub left: u64,
    /// Total torrent length.
    pub size: u64,
}

/// What an HTTP announce says about peer-wire encryption (MSE/PE): `supportcrypto=1` and
/// `requirecrypto=1` (SPEC §12 #18). UDP announces (BEP 15) have no such field and ignore it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum Crypto {
    /// Say nothing (encryption off).
    #[default]
    Off,
    /// `supportcrypto=1` (encryption preferred).
    Support,
    /// `supportcrypto=1&requirecrypto=1` (encryption required).
    Require,
}

/// Everything that goes into an announce, independent of transport.
///
/// size: ~140 bytes plus the optional tracker id allocation (short-lived: one
/// per request).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AnnounceParams {
    /// 20-byte info-hash.
    pub info_hash: InfoHash20,
    /// Our peer-id (stable for the whole process session).
    pub peer_id: PeerId20,
    /// Listening port.
    pub port: u16,
    /// Uploaded payload bytes (session).
    pub uploaded: u64,
    /// Downloaded payload bytes (session).
    pub downloaded: u64,
    /// Bytes left.
    pub left: u64,
    /// 32-bit `key` identifying us across IP changes.
    pub key: u32,
    /// Event, `None` for a regular announce.
    pub event: Option<Event>,
    /// Peers wanted (0 for `stopped`).
    pub numwant: u32,
    /// Echo of the tracker's `tracker id`, once it sent one.
    pub tracker_id: Option<Box<[u8]>>,
    /// `ipv4=`: only when the user explicitly configured it (default `None`;
    /// mainstream clients (qBittorrent, Transmission) do not send it unless they know a public address).
    pub ipv4: Option<Ipv4Addr>,
    /// `ipv6=` (BEP 7): only when the user explicitly configured it.
    pub ipv6: Option<Ipv6Addr>,
    /// `supportcrypto` / `requirecrypto` (HTTP only).
    pub crypto: Crypto,
}

/// A peer returned by a tracker.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Peer {
    /// Address and port.
    pub addr: SocketAddr,
    /// Peer-id when the tracker sent the non-compact form.
    pub peer_id: Option<PeerId20>,
}

impl Peer {
    /// IP of the peer.
    pub fn ip(&self) -> IpAddr {
        self.addr.ip()
    }
}

/// What the torrent is doing, as far as retry pace is concerned (SPEC 7C.4).
///
/// A download needs peers soon; a seed does not, and hundreds of seeds retrying
/// quickly would waste wake-ups and tracker goodwill. Regular announces follow
/// the tracker's interval for every role (a seed must stay listed); the role
/// only sets the pace of *retries after a transient failure*.
///
/// | Role | Schedule (+0-20 % jitter) | Cap |
/// |---|---|---|
/// | `Downloading` (incl. fetching metadata) | 1, 2, 4, 8, 16, 32 min | 1 h |
/// | `Seeding` | 15 min, 30 min, 1 h, 2 h, 4 h | 6 h |
/// | `SeedingDormant` | 30 min, 1 h, 2 h, 4 h, 8 h | 12 h |
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum TorrentRole {
    /// Still downloading (or fetching metadata). The default.
    #[default]
    Downloading,
    /// Seeding (popular or active).
    Seeding,
    /// Seeding, but dormant (nobody wants the data).
    SeedingDormant,
}

/// Which kind of torrent event the engine reports.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TorrentEvent {
    /// Torrent starts (or restarts) transferring.
    Start,
    /// Download finished.
    Complete,
    /// Torrent removed or app quitting.
    Stop,
    /// User paused.
    Pause,
    /// User resumed.
    Resume,
}
