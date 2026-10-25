//! Result types of the protocol. Plain data with serde; the backend adapter fills them from the
//! engine's records, the server adds the human-readable strings, the CLI reads them.
//!
//! New fields are added as `Option`/`#[serde(default)]` so older clients keep working.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::human;

/// A transfer id: the 20-byte swarm id of a torrent, or the id of a direct download.
pub type TransferId = [u8; 20];

/// Lower-case hex of an id.
pub fn id_hex(id: &TransferId) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(40);
    for b in id {
        s.push(HEX[usize::from(b >> 4)] as char);
        s.push(HEX[usize::from(b & 15)] as char);
    }
    s
}

/// Parse 40 hex characters.
pub fn parse_id(s: &str) -> Option<TransferId> {
    let b = s.as_bytes();
    if b.len() != 40 {
        return None;
    }
    let nib = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
    let mut out = [0u8; 20];
    for (i, o) in out.iter_mut().enumerate() {
        *o = nib(b[2 * i])? << 4 | nib(b[2 * i + 1])?;
    }
    Some(out)
}

/// The first 8 characters of an id (all of it when shorter), without ever splitting a character.
pub fn short(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

fn is_zero_u32(v: &u32) -> bool {
    *v == 0
}

fn is_zero_u16(v: &u16) -> bool {
    *v == 0
}

/// Seeding goal as sent over the wire.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SeedGoalDto {
    /// Ratio target.
    #[serde(default)]
    pub ratio: Option<f32>,
    /// Seconds-of-seeding target.
    #[serde(default)]
    pub seed_secs: Option<u32>,
    /// Hit-and-run floor.
    #[serde(default)]
    pub min_seed_secs: u32,
    /// First reached wins (true) or all must be reached (false).
    #[serde(default)]
    pub any: bool,
    /// `stop`, `remove` or `remove_and_delete`.
    #[serde(default)]
    pub at_goal: String,
    /// Stop when the download finishes.
    #[serde(default)]
    pub stop_on_finish: bool,
    /// The inactive condition (SPEC §7.5): stop below this many MB uploaded in the last `idle_hours`
    /// hours of seeding; 0 = off.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub idle_mb: u32,
    /// Window of `idle_mb`, hours.
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub idle_hours: u16,
    /// The goal in words, from the engine ("Seed to ratio 1.0"): the same text in the app and the CLI.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
}

/// Pre-formatted strings for people; the raw numbers are next to them in [`TransferDto`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Human {
    /// Wanted size, `1.5 GB`.
    #[serde(default)]
    pub size: String,
    /// Verified bytes.
    #[serde(default)]
    pub done: String,
    /// Download rate.
    #[serde(default)]
    pub down_rate: String,
    /// Upload rate.
    #[serde(default)]
    pub up_rate: String,
    /// Time left, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eta: Option<String>,
    /// Progress, `42.5%`.
    #[serde(default)]
    pub progress: String,
    /// Share ratio, `1.23`.
    #[serde(default)]
    pub ratio: String,
}

/// One transfer (a row of the engine snapshot).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TransferDto {
    /// Id (40 hex characters).
    pub id: String,
    /// `torrent` or `direct`.
    pub kind: String,
    /// The raw name (release name or file name).
    pub name: String,
    /// The clean title ("Sintel (2010)"), computed once by the engine and stored with the torrent;
    /// empty until the files are known. Show this when present, `name` otherwise.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    /// `checking`, `metadata`, `queued`, `downloading`, `seeding`, `low-power`, `paused`,
    /// `missing-data`, `error`, `completed`, `awaiting-choice` (held until `choose_files`),
    /// `pending` (held for the grace period before an auto-add starts, see `pending`).
    pub state: String,
    /// Fraction of the wanted bytes verified, 0..=1.
    pub progress: f64,
    /// Whole size in bytes (0 until metadata).
    pub total_bytes: u64,
    /// Bytes of the files the user wants.
    pub wanted_bytes: u64,
    /// Verified bytes.
    pub done_bytes: u64,
    /// Bytes/s down.
    pub down_rate: u64,
    /// Bytes/s up.
    pub up_rate: u64,
    /// Connected peers.
    pub peers: u32,
    /// Connected seeds.
    pub seeds: u32,
    /// Peer addresses known for the torrent: tracker and incoming connections (private torrents; plus
    /// PEX when the tracker allows it); plus DHT and PEX for public ones. The pool the connection
    /// manager draws from. Shown as `peers / peers_known`.
    #[serde(default)]
    pub peers_known: u32,
    /// Seconds to completion, when estimable.
    #[serde(default)]
    pub eta_secs: Option<u64>,
    /// Lifetime upload / size.
    pub ratio: f64,
    /// Host of the first tracker (never a path or query).
    pub tracker_host: String,
    /// Tracker profile id, if one matched.
    #[serde(default)]
    pub tracker_profile: Option<String>,
    /// `cold`, `warm` or `hot` (power tier).
    pub tier: String,
    /// BEP 27 private torrent.
    pub private: bool,
    /// Unix seconds added.
    pub added_at: u64,
    /// Unix seconds finished.
    #[serde(default)]
    pub finished_at: Option<u64>,
    /// Error text.
    #[serde(default)]
    pub error: Option<String>,
    /// Where the files are.
    pub save_path: String,
    /// `high`, `normal`, `low`.
    pub priority: String,
    /// Queue position (0 = front).
    pub queue_pos: u32,
    /// Lifetime bytes uploaded.
    pub uploaded_total: u64,
    /// Lifetime bytes downloaded.
    pub downloaded_total: u64,
    /// Category.
    #[serde(default)]
    pub category: Option<String>,
    /// Seeding goal in effect.
    pub seed_goal: SeedGoalDto,
    /// The goal is this torrent's own (`set_seed_goal`), not its rule's; `set_seed_goal` with
    /// `rule_default` puts it back.
    #[serde(default)]
    pub seed_goal_user: bool,
    /// The inactive condition's live figure, from the engine: "Uploaded in the last 150 h: 0.3 GB
    /// (stops below 1 GB)". Absent when the condition is off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle: Option<String>,
    /// Why the torrent stopped when the inactive condition did it: "0.3 GB uploaded in the last 150 h".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_stopped: Option<String>,
    /// Seconds spent seeding in total.
    #[serde(default)]
    pub seeded_secs: u64,
    /// Rule forced for this transfer.
    #[serde(default)]
    pub rule: Option<String>,
    /// Source URL of a direct download, query and secrets redacted.
    #[serde(default)]
    pub url: Option<String>,
    /// Per-transfer upload limit from its rule.
    #[serde(default)]
    pub up_limit: Option<u64>,
    /// The files have paths of their own (placed by an earlier version, SPEC 7A.3), not the torrent's layout.
    #[serde(default)]
    pub organized: bool,
    /// Unix seconds of the last upload or interested peer (finished torrents).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity: Option<u64>,
    /// Unix seconds when the current error began.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_since: Option<u64>,
    /// Unix seconds of the next attempt after a tracker error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_retry_at: Option<u64>,
    /// Why a download is not moving ("Tracker: Unregistered torrent", "Waiting for peers"); the
    /// same words the app's row shows. Absent while data flows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_note: Option<String>,
    /// The files are being moved to another location: progress 0..=1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub moving: Option<f32>,
    /// Why the last automatic move into a status folder was not done ("“f22.bin” is locked ...");
    /// the torrent keeps seeding where it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub move_note: Option<String>,
    /// While `state` is `pending`: when it starts. Nothing is announced or downloaded until then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<PendingDto>,
    /// Human-readable strings (filled by the server).
    #[serde(default)]
    pub human: Human,
}

impl TransferDto {
    /// Fill [`Human`] from the raw numbers.
    pub fn fill_human(&mut self) {
        self.human = Human {
            size: human::bytes(self.wanted_bytes),
            done: human::bytes(self.done_bytes),
            down_rate: human::rate(self.down_rate),
            up_rate: human::rate(self.up_rate),
            eta: self.eta_secs.map(human::duration),
            progress: human::percent(self.progress),
            ratio: format!("{:.2}", self.ratio),
        };
    }

    /// The first 8 characters of the id, for compact output.
    pub fn short_id(&self) -> &str {
        short(&self.id)
    }

    /// Seeding (including low power).
    pub fn is_sharing(&self) -> bool {
        matches!(self.state.as_str(), "seeding" | "low-power")
    }

    /// Done downloading what it was asked for.
    pub fn is_done(&self) -> bool {
        self.finished_at.is_some() || matches!(self.state.as_str(), "seeding" | "low-power" | "completed")
    }
}

/// Global limits.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LimitsDto {
    /// Effective download limit (alternative limits applied), bytes/s; null = unlimited.
    pub down_limit: Option<u64>,
    /// Effective upload limit.
    pub up_limit: Option<u64>,
    /// Alternative limits in force (manual toggle or schedule).
    pub alt_active: bool,
    /// ...because the schedule says so.
    #[serde(default)]
    pub alt_scheduled: bool,
    /// Download limit used while alternative speed is on.
    #[serde(default)]
    pub alt_down_limit: Option<u64>,
    /// Upload limit used while alternative speed is on.
    #[serde(default)]
    pub alt_up_limit: Option<u64>,
}

/// Engine totals that come with a snapshot.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TotalsDto {
    /// Global download rate.
    pub down_rate: u64,
    /// Global upload rate.
    pub up_rate: u64,
    /// Open peer connections.
    pub connections: u32,
    /// TCP port in use.
    pub listen_port: u16,
    /// Torrents in the Hot tier.
    pub hot: u32,
    /// Torrents in the Warm tier.
    pub warm: u32,
    /// Torrents in the Cold tier.
    pub cold: u32,
}

/// What the backend returns for `snapshot`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SnapshotDto {
    /// Rows in queue order.
    pub transfers: Vec<TransferDto>,
    /// Totals.
    pub totals: TotalsDto,
}

/// `status` result.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StatusDto {
    /// `seedwell <version>`.
    pub server: String,
    /// Protocol version.
    pub protocol: u32,
    /// Agent permission level in force.
    pub permission: String,
    /// Global rates and connections.
    pub totals: TotalsDto,
    /// Transfers by state name.
    pub counts: BTreeMap<String, u32>,
    /// Number of transfers.
    pub total: u32,
    /// Limits in effect.
    pub limits: LimitsDto,
    /// `1.2 MB/s down, 300 KB/s up, 3 downloading, 40 sharing`.
    pub summary: String,
    /// Can peers on the Internet reach the listening port (absent from a backend without an engine).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reachability: Option<ReachabilityDto>,
}

/// Can peers on the Internet reach the listening port? Built by [`crate::ops::reachability`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReachabilityDto {
    /// `open` (a peer from the Internet connected in), `mapped` (the router opened the port; no
    /// peer has connected in yet), `checking` (the router is being asked), `closed` (the router
    /// cannot open it) or `unknown` (port mapping is off and no peer has connected in).
    pub state: String,
    /// One line for people: "Open: 3 peers from the Internet connected in".
    pub summary: String,
    /// The TCP port we listen on.
    pub port: u16,
    /// Port mapping is switched on.
    pub mapping: bool,
    /// `pcp`, `nat-pmp` or `upnp` once the router answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// The TCP port the router opened (can differ from `port`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_port: Option<u16>,
    /// The router's own address is not on the Internet (CGNAT): no port can be opened there.
    pub behind_cgnat: bool,
    /// The router answered none of PCP, NAT-PMP and UPnP.
    pub router_unavailable: bool,
    /// Incoming TCP connections from public IPv4 addresses since the port was opened.
    pub incoming: u64,
    /// When the last of them came in (Unix seconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_incoming_at: Option<u64>,
    /// Folder sync's TCP port as the router opened it (when sync runs and mapping is on).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_external_port: Option<u16>,
    /// Folder sync's UDP port (uTP) as the router opened it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_external_udp_port: Option<u16>,
}

/// `list` result.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ListDto {
    /// Rows matching the filter.
    pub total: u32,
    /// Rows skipped.
    pub offset: u32,
    /// The page.
    pub transfers: Vec<TransferDto>,
}

/// One file of a transfer.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FileDto {
    /// Index in torrent order (what `set_file_priorities` uses).
    pub index: u32,
    /// Path inside the torrent.
    pub path: String,
    /// Length.
    pub length: u64,
    /// Verified bytes (approximate for files sharing pieces).
    pub done: u64,
    /// `skip`, `low`, `normal`, `high`.
    pub priority: String,
    /// BEP 47 padding file.
    #[serde(default)]
    pub padding: bool,
}

/// One tracker of a transfer. URLs have passkeys removed; timing is coarse (Unix seconds).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TrackerDto {
    /// Redacted announce URL.
    pub url: String,
    /// Tier index.
    pub tier: u32,
    /// Status text.
    pub status: String,
    /// Seeders the tracker reports.
    #[serde(default)]
    pub seeds: Option<u64>,
    /// Leechers the tracker reports.
    #[serde(default)]
    pub leechers: Option<u64>,
    /// Unix seconds of the last announce.
    #[serde(default)]
    pub last_announce_at: Option<u64>,
    /// Unix seconds of the next regular announce.
    #[serde(default)]
    pub next_announce_at: Option<u64>,
    /// `waiting`, `working`, `warning`, `backoff` or `halted`.
    #[serde(default)]
    pub state: String,
    /// Consecutive failures.
    #[serde(default)]
    pub failures: u32,
    /// Unix seconds of the last announce the tracker answered.
    #[serde(default)]
    pub last_success_at: Option<u64>,
    /// Unix seconds at which a failed announce is tried again.
    #[serde(default)]
    pub retry_at: Option<u64>,
    /// Unix seconds before which `debug announce` is held back (the tracker's minimum interval).
    #[serde(default)]
    pub manual_ok_at: Option<u64>,
    /// Why the engine's circuit breaker holds this tracker back.
    #[serde(default)]
    pub tripped: Option<String>,
}

/// One connected peer. Addresses appear here only.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PeerDto {
    /// `ip:port`.
    pub addr: String,
    /// Client name.
    pub client: String,
    /// We connected out.
    pub outgoing: bool,
    /// Bytes/s from the peer.
    pub down_rate: u64,
    /// Bytes/s to the peer.
    pub up_rate: u64,
    /// Fraction of pieces the peer has.
    pub progress: f32,
    /// They choke us.
    pub choking_us: bool,
    /// We choke them.
    pub choked_by_us: bool,
    /// They are interested.
    pub interested: bool,
    /// The connection runs over uTP (otherwise TCP).
    #[serde(default)]
    pub utp: bool,
    /// Block requests sent and not yet answered.
    #[serde(default)]
    pub requests_in_flight: u32,
    /// Seconds since the connection was made.
    #[serde(default)]
    pub connected_secs: u64,
    /// We stopped asking this peer for data because it did not deliver.
    #[serde(default)]
    pub snubbed: bool,
    /// The peer is a seed that wants nothing.
    #[serde(default)]
    pub upload_only: bool,
    /// The peer does peer exchange.
    #[serde(default)]
    pub has_pex: bool,
}

/// `info` result.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DetailDto {
    /// The list row.
    pub transfer: TransferDto,
    /// Files (empty when not requested or direct download).
    #[serde(default)]
    pub files: Vec<FileDto>,
    /// More files exist than returned.
    #[serde(default)]
    pub files_truncated: u32,
    /// Trackers.
    #[serde(default)]
    pub trackers: Vec<TrackerDto>,
    /// Peers.
    #[serde(default)]
    pub peers: Vec<PeerDto>,
    /// More peers exist than returned.
    #[serde(default)]
    pub peers_truncated: u32,
    /// Piece count.
    #[serde(default)]
    pub piece_count: u32,
    /// Piece length.
    #[serde(default)]
    pub piece_length: u32,
    /// Torrent comment.
    #[serde(default)]
    pub comment: Option<String>,
}

/// An `{id, name}` pair.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct IdName {
    /// Id.
    pub id: String,
    /// Name.
    pub name: String,
}

/// Result of a command that acts on several transfers.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ActionDto {
    /// What was done (`paused`, `resumed`, ...).
    pub action: String,
    /// How many transfers were addressed.
    pub count: u32,
    /// Which.
    pub transfers: Vec<IdName>,
}

/// The grace period before an auto-added torrent starts (SPEC 7A.7).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingDto {
    /// Unix milliseconds at which it starts (meaningless while `paused`).
    pub starts_at_ms: u64,
    /// Seconds left, rounded up.
    pub starts_in_secs: u64,
    /// The countdown is stopped by the user; the torrent stays held.
    #[serde(default)]
    pub paused: bool,
}

/// `add` result.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AddedDto {
    /// Id of the new transfer.
    pub id: String,
    /// Name (the magnet's `dn`, or the id until metadata arrives).
    pub name: String,
    /// `torrent` or `direct`.
    pub kind: String,
    /// The torrent is held until the caller chooses what to download (`choose_files`, or
    /// `cancel_choice`): nothing is announced or downloaded meanwhile.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub needs_choice: bool,
    /// The structure to choose from, when `needs_choice` (a magnet is held later, once its
    /// metadata is in: watch for the `choice_needed` event, then call `choice`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub choice: Option<ChoiceDto>,
    /// The torrent waits out the grace period (`transfers.start_delay_secs`) before it starts, and
    /// is silent on the network until then. Add with `now: true` to skip the wait.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<PendingDto>,
}

/// Why a torrent needs a choice.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ChoiceReasonDto {
    /// `several_items`, `in_library`, `platform_variants`, `very_large`, `not_enough_space`,
    /// `unsure`, `folder_ask`, `always`.
    pub kind: String,
    /// One plain sentence.
    pub text: String,
}

/// One selectable thing (an episode, a film, an album, a title, a build).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ChoiceItemDto {
    /// `S03E01`, `Night of the Living Dead (1968)`, ...
    pub label: String,
    /// Bytes.
    pub bytes: u64,
    /// File indices (as in `info`).
    pub files: Vec<u32>,
    /// Ticked in the engine's preselection.
    pub selected: bool,
    /// `missing`, `in_library` (same file already at the destination) or `other_quality`.
    pub library: String,
    /// `1080p WEB-DL`, `FLAC`, ...
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality: Option<String>,
    /// Software: the build for this Mac.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub this_mac: bool,
    /// Software: `macos arm64`, `windows x86_64`, ...
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
}

/// A group of items: a season, a platform, "Extras".
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ChoiceGroupDto {
    /// `Season 3`, `Windows`, `Extras`, ...
    pub label: String,
    /// `season`, `specials`, `collection`, `album`, `platform`, `extras`, `other`.
    pub kind: String,
    /// Items.
    pub items: Vec<ChoiceItemDto>,
}

/// The chooser of a torrent: structure, preselection and reasons (SPEC 7A.6).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ChoiceDto {
    /// The torrent is held (state `awaiting-choice`).
    pub awaiting: bool,
    /// The engine would hold it for a choice.
    pub needs_choice: bool,
    /// Why.
    pub reasons: Vec<ChoiceReasonDto>,
    /// `single`, `tv`, `movies`, `music`, `books`, `audiobooks`, `software`, `flat`.
    pub structure: String,
    /// Groups of items (the last ones are "Other files" and "Extras").
    pub groups: Vec<ChoiceGroupDto>,
    /// Preselection per file index.
    pub files: Vec<bool>,
    /// Bytes of the preselection.
    pub selected_bytes: u64,
    /// Files of the preselection.
    pub selected_files: u32,
    /// Whole size.
    pub total_bytes: u64,
    /// Free bytes at the destination.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free_bytes: Option<u64>,
    /// Where it would be saved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination: Option<String>,
    /// Category in effect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// Classifier confidence 0..=1.
    pub confidence: f32,
    /// A sentence for people: "TV, 6 items, 1.2 GB selected of 1.3 GB".
    pub summary: String,
    /// Name of the rule the torrent gets in this category ("Default" when none matches).
    #[serde(default)]
    pub rule: String,
    /// What the rule does when done ("seeds until ratio 1.0").
    #[serde(default)]
    pub then: String,
    /// "Rule Y - seed to 1.0 · Saves to ~/Media/Movies · Then seeds until ratio 1.0".
    #[serde(default)]
    pub rule_summary: String,
}

/// One torrent a location change would move, or a queued move.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MoveItemDto {
    /// Id.
    pub id: String,
    /// Name.
    pub name: String,
    /// Bytes to relocate.
    pub bytes: u64,
    /// Where the content is.
    pub from: String,
    /// Where it would go.
    pub to: String,
    /// `true`: a verified copy across volumes; `false`: an instant rename.
    pub cross_volume: bool,
    /// `finished`, `incomplete` or `retarget`.
    pub kind: String,
}

/// `location_moves` result.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MoveProposalDto {
    /// Torrents that sit elsewhere than the settings say.
    pub items: Vec<MoveItemDto>,
    /// Their bytes.
    pub total_bytes: u64,
    /// How many cross volumes.
    pub cross_volume: u32,
}

/// `move` result.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MoveReportDto {
    /// Queued (they move one after the other; watch `moving` on the transfer and the
    /// `move_*` events).
    pub queued: Vec<IdName>,
    /// Not queued, with the reason.
    pub refused: Vec<MoveRefused>,
}

/// A transfer that was not moved.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MoveRefused {
    /// Id.
    pub id: String,
    /// Why.
    pub reason: String,
}

/// One tracker's answer to `reannounce`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ReannounceTracker {
    /// Redacted URL.
    pub url: String,
    /// Status text.
    pub status: String,
    /// Unix seconds before which this tracker will not be asked again, when known.
    #[serde(default)]
    pub not_before: Option<u64>,
    /// With `wait`: what the tracker answered ("ok: 12 seeders, 3 leechers") or the plain failure.
    #[serde(default)]
    pub answer: Option<String>,
}

/// `reannounce` result for one transfer.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ReannounceDto {
    /// Transfer.
    pub id: String,
    /// Name.
    pub name: String,
    /// The trackers' state after the request.
    pub trackers: Vec<ReannounceTracker>,
    /// Plain-language outcome.
    pub note: String,
}

/// `stream_url` result: one file served over loopback HTTP (`Range` supported) while it downloads.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StreamInfoDto {
    /// `http://127.0.0.1:<port>/<token>/<name>`; absent when the file is `complete`: open the file itself.
    pub url: Option<String>,
    /// Pass to `stop_stream`.
    pub token: Option<String>,
    /// Transfer.
    pub id: String,
    /// File index in the transfer.
    pub file: u32,
    /// File name (last path component).
    pub file_name: String,
    /// Bytes.
    pub file_len: u64,
    /// MIME type from the extension.
    pub content_type: String,
    /// `mp4_family`, `matroska`, `avi`, `mpeg_ts`, `other_video`, `audio` or `unknown`.
    pub container: String,
    /// QuickTime Player (AVFoundation) can play it.
    pub is_mp4_family: bool,
    /// Every piece of the file is verified already.
    pub complete: bool,
    /// The file had been skipped and is now selected for download.
    pub selected: bool,
}

/// `remove` result (step 1).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RemovePlanDto {
    /// Pass this to `remove_confirm`.
    pub token: String,
    /// Seconds the token stays valid.
    pub expires_in_secs: u64,
    /// Whether downloaded files will be deleted.
    pub delete_files: bool,
    /// What will be removed.
    pub items: Vec<RemoveItem>,
    /// Plain-language summary to show a person.
    pub summary: String,
}

/// One transfer in a removal plan.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RemoveItem {
    /// Id.
    pub id: String,
    /// Name.
    pub name: String,
    /// `torrent` or `direct`.
    pub kind: String,
    /// Bytes on disk that deletion would remove (verified bytes).
    pub bytes_on_disk: u64,
    /// Where the files are.
    pub path: String,
    /// State.
    pub state: String,
}

/// `remove_confirm` result.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RemovedDto {
    /// Removed transfers.
    pub removed: Vec<IdName>,
    /// Planned transfers that were already gone.
    pub missing: Vec<String>,
    /// Files were deleted.
    pub files_deleted: bool,
}

/// One tracker: a favorite (a stored profile) or an automatically identified one.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ProfileDto {
    /// A favorite (stored in `trackers.toml`, can have settings) rather than an automatic tracker.
    #[serde(default)]
    pub favorite: bool,
    /// Id.
    pub id: String,
    /// Name.
    pub name: String,
    /// Announce URL patterns (no passkeys by construction).
    pub urls: Vec<String>,
    /// Linked rule.
    #[serde(default)]
    pub rule: Option<String>,
    /// Shared download cap of a favorite (all its torrents together), bytes/s; null = unlimited.
    #[serde(default)]
    pub down_limit: Option<u64>,
    /// Shared upload cap of a favorite, bytes/s; null = unlimited.
    #[serde(default)]
    pub up_limit: Option<u64>,
    /// At least one of its transfers is private: only then is there a peer exchange setting.
    #[serde(default)]
    pub private: bool,
    /// Peer exchange (BEP 11) is allowed for its private torrents (`set_tracker_pex`). Off by default.
    #[serde(default)]
    pub allow_pex: bool,
    /// Transfers currently in this profile.
    pub transfers: u32,
    /// Of which sharing now.
    pub sharing: u32,
    /// Current upload rate of the profile's transfers.
    pub up_rate: u64,
    /// Current download rate.
    pub down_rate: u64,
}

/// `trackers` result.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TrackersDto {
    /// Favorites (stored profiles) in file order.
    pub profiles: Vec<ProfileDto>,
    /// Automatically identified trackers (registrable domain of the primary announce host) of
    /// the transfers no favorite claims, busiest first. `id` is the domain, `urls` its hosts.
    #[serde(default)]
    pub groups: Vec<ProfileDto>,
    /// Transfers without any tracker that are public.
    pub public_transfers: u32,
    /// Transfers without a profile that are private.
    pub other_private_transfers: u32,
    /// Peer exchange is allowed for those (the `other-private` tracker).
    #[serde(default)]
    pub other_private_allow_pex: bool,
    /// The master switch (`network.pex`): off means no peer exchange for any torrent, whatever
    /// the trackers allow.
    #[serde(default)]
    pub pex_enabled: bool,
}

/// Counters of one day or a period.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BucketDto {
    /// Payload bytes downloaded.
    pub downloaded: u64,
    /// Payload bytes uploaded.
    pub uploaded: u64,
    /// `uploaded / downloaded`, null when nothing was downloaded.
    pub ratio: Option<f64>,
    /// Torrents added.
    pub torrents_added: u64,
    /// Torrents completed.
    pub torrents_completed: u64,
    /// Torrents removed.
    pub torrents_removed: u64,
    /// Files completed.
    pub files_completed: u64,
    /// Seconds of seeding.
    pub seed_secs: u64,
}

/// One day of the series.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DayDto {
    /// `YYYY-MM-DD` (local day).
    pub date: String,
    /// Counters.
    #[serde(flatten)]
    pub bucket: BucketDto,
}

/// `tracker_stats` result.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TrackerStatsDto {
    /// The scope: profile id, `public`, `other-private` or `all`.
    pub tracker: String,
    /// Today.
    pub today: BucketDto,
    /// Monday through today.
    pub this_week: BucketDto,
    /// The 1st through today.
    pub this_month: BucketDto,
    /// Lifetime.
    pub all_time: BucketDto,
    /// Daily series, oldest first.
    pub series: Vec<DayDto>,
    /// `1.2 GB up, 300 MB down this month` style summary.
    pub summary: String,
}

/// `wait_for` result.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct WaitDto {
    /// The condition was met.
    pub done: bool,
    /// The wait ended because the time ran out.
    pub timed_out: bool,
    /// The addressed transfers as they are now.
    pub transfers: Vec<TransferDto>,
}

/// An event, pushed or polled.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct EventDto {
    /// Monotonic sequence number assigned by the server.
    #[serde(default)]
    pub seq: u64,
    /// Unix milliseconds.
    #[serde(default)]
    pub at: u64,
    /// `added`, `metadata_received`, `finished`, `state_changed`, `error`,
    /// `seeding_goal_reached`, `removed`, `auto_added`, `duplicate`,
    /// `direct_restarted`, `watch_error`, `config_reloaded`, `config_rejected`, `lagged`,
    /// `choice_needed`, `choice_resolved`, `pending` (`data.change`: added, paused, resumed, started, chosen), `locations_changed`, `move_started`, `move_progress`,
    /// `move_finished`, `move_failed`.
    pub kind: String,
    /// Transfer id, when the event is about one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Transfer name, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// More detail (new state, message, ...).
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub data: Value,
}

/// `events` result.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct EventsDto {
    /// Events with `seq` > `since`, oldest first.
    pub events: Vec<EventDto>,
    /// Pass this as `since` next time.
    pub cursor: u64,
    /// Events before this `seq` were dropped from the buffer.
    pub oldest_available: u64,
}

/// One entry of the agent activity log.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ActivityDto {
    /// Unix milliseconds.
    pub at: u64,
    /// Client name from `hello`.
    pub client: String,
    /// Method.
    pub method: String,
    /// Redacted description of the arguments.
    pub summary: String,
    /// `ok`, `denied` or `error: <code>`.
    pub outcome: String,
}

/// `hello` result.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct HelloDto {
    /// Protocol version the server speaks.
    pub protocol: u32,
    /// `seedwell <version>`.
    pub server: String,
    /// Permission level in force right now.
    pub permission: String,
    /// Methods this connection may call at this level.
    pub methods: Vec<String>,
    /// Per-connection rate limit, calls per second.
    pub rate_limit_per_sec: u32,
}

/// `settings_get` / `settings_set` result.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SettingsDto {
    /// Dotted key to value.
    pub settings: serde_json::Map<String, Value>,
    /// Keys that were changed (`settings_set`).
    #[serde(default)]
    pub changed: Vec<String>,
    /// Note for the user ("applied within a few seconds").
    #[serde(default)]
    pub note: String,
}

/// Name of a transfer state counted in `status.counts`, in display order.
pub const STATE_ORDER: [&str; 10] = [
    "downloading",
    "queued",
    "checking",
    "metadata",
    "seeding",
    "low-power",
    "paused",
    "completed",
    "missing-data",
    "error",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_roundtrip() {
        let id = [0xab; 20];
        let h = id_hex(&id);
        assert_eq!(h.len(), 40);
        assert_eq!(parse_id(&h), Some(id));
        assert_eq!(parse_id(&h.to_uppercase()), Some(id));
        assert_eq!(parse_id("zz"), None);
        assert_eq!(parse_id(&"g".repeat(40)), None);
        assert_eq!(parse_id(&"é".repeat(20)), None);
    }

    #[test]
    fn human_fills() {
        let mut t = TransferDto { wanted_bytes: 1_500_000, done_bytes: 750_000, progress: 0.5, eta_secs: Some(90), ..Default::default() };
        t.fill_human();
        assert_eq!(t.human.size, "1.5 MB");
        assert_eq!(t.human.progress, "50.0%");
        assert_eq!(t.human.eta.as_deref(), Some("1m 30s"));
    }

    #[test]
    fn old_clients_ignore_new_fields_and_new_ignore_missing() {
        // A newer server adds a field; an older DTO still parses.
        let j = r#"{"id":"a","kind":"torrent","name":"n","state":"seeding","progress":1.0,"total_bytes":1,"wanted_bytes":1,"done_bytes":1,"down_rate":0,"up_rate":0,"peers":0,"seeds":0,"ratio":0.0,"tracker_host":"","tier":"cold","private":false,"added_at":1,"save_path":"/x","priority":"normal","queue_pos":0,"uploaded_total":0,"downloaded_total":0,"seed_goal":{},"future_field":42}"#;
        let t: TransferDto = serde_json::from_str(j).unwrap();
        assert_eq!(t.state, "seeding");
    }
}

// ----------------------------------------------------------------------------- diagnostics

/// What the engine can say about its gates and network, read on demand (`debug_perf`, `debug_health`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ProbeDto {
    /// False while the engine has declared a network outage.
    pub online: bool,
    /// Unix seconds at which the outage began.
    #[serde(default)]
    pub offline_since: Option<u64>,
    /// TCP (and uTP, DHT) port in use.
    pub listen_port: u16,
    /// The configured port was taken; the text says which one is used instead.
    #[serde(default)]
    pub port_fallback: Option<String>,
    /// Global download limiter is on.
    pub down_limited: bool,
    /// Global upload limiter is on.
    pub up_limited: bool,
    /// Favorite trackers with a shared cap.
    pub tracker_caps: u32,
    /// Verification slots in total.
    pub verify_slots: u32,
    /// Verification slots free.
    pub verify_free: u32,
    /// Connection cap.
    pub max_connections: u32,
    /// Half-open cap.
    pub max_half_open: u32,
    /// Connections open, handshaking ones included.
    pub connections_open: u32,
    /// Handshakes pending.
    pub half_open: u32,
    /// The DHT runs.
    pub dht_running: bool,
    /// `pcp`, `nat-pmp` or `upnp` once a gateway answered.
    #[serde(default)]
    pub portmap_method: Option<String>,
    /// The port mapper reported at all yet.
    pub portmap_reported: bool,
    /// No method worked.
    pub portmap_unavailable: bool,
    /// The router's address is not reachable from the Internet.
    pub portmap_behind_cgnat: bool,
    /// TCP port as seen from outside.
    #[serde(default)]
    pub external_port: Option<u16>,
    /// Engine counters since it started.
    pub counters: BTreeMap<String, u64>,
}

/// The server process (the app or the daemon), measured on demand.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ProcessDto {
    /// Process id.
    pub pid: u32,
    /// Resident memory in bytes (`ps`), when it could be read.
    #[serde(default)]
    pub rss_bytes: Option<u64>,
    /// CPU percent over the process lifetime window `ps` reports.
    #[serde(default)]
    pub cpu_percent: Option<f64>,
    /// Threads.
    #[serde(default)]
    pub threads: Option<u32>,
    /// Open file descriptors (sockets included).
    #[serde(default)]
    pub open_fds: Option<u32>,
}

/// One tracker of one torrent, for `debug trackers`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DebugTrackerDto {
    /// Announce URL, secrets masked.
    pub url: String,
    /// Tier.
    pub tier: u32,
    /// `waiting`, `working`, `warning`, `backoff`, `halted`.
    pub state: String,
    /// The tracker's last words or our last error.
    pub status: String,
    /// Consecutive failures.
    pub failures: u32,
    /// Unix seconds of the last announce sent.
    #[serde(default)]
    pub last_announce_at: Option<u64>,
    /// Unix seconds of the last answered announce.
    #[serde(default)]
    pub last_success_at: Option<u64>,
    /// Seeders the tracker reported.
    #[serde(default)]
    pub seeders: Option<u64>,
    /// Leechers the tracker reported.
    #[serde(default)]
    pub leechers: Option<u64>,
    /// Unix seconds of the next announce.
    #[serde(default)]
    pub next_announce_at: Option<u64>,
    /// Why that is the next time.
    pub next_reason: String,
    /// Unix seconds before which `debug announce` is held back.
    #[serde(default)]
    pub manual_ok_at: Option<u64>,
    /// The circuit breaker's reason, if it holds the tracker back.
    #[serde(default)]
    pub tripped: Option<String>,
}

/// One torrent's trackers.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DebugTorrentTrackers {
    /// Transfer id.
    pub id: String,
    /// Name.
    pub name: String,
    /// State name.
    pub state: String,
    /// The trackers.
    pub trackers: Vec<DebugTrackerDto>,
}

/// `debug_trackers` result.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DebugTrackersDto {
    /// Network state: false while offline (trackers are held, not failing).
    pub online: bool,
    /// Unix seconds at which the outage began.
    #[serde(default)]
    pub offline_since: Option<u64>,
    /// Torrents with their trackers.
    pub transfers: Vec<DebugTorrentTrackers>,
    /// More torrents exist than are shown.
    pub truncated: u32,
}

/// One peer for `debug peers`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DebugPeerDto {
    /// `ip:port`.
    pub addr: String,
    /// Client name.
    pub client: String,
    /// `tcp` or `utp`.
    pub transport: String,
    /// `out` (we dialled) or `in`.
    pub direction: String,
    /// They choke us.
    pub choking_us: bool,
    /// We choke them.
    pub choked_by_us: bool,
    /// They are interested in us.
    pub interested: bool,
    /// Bytes/s from the peer.
    pub down_rate: u64,
    /// Bytes/s to the peer.
    pub up_rate: u64,
    /// Block requests in flight.
    pub requests_in_flight: u32,
    /// Fraction of pieces the peer has.
    pub progress: f32,
    /// Seconds connected.
    pub connected_secs: u64,
    /// Snubbed.
    pub snubbed: bool,
    /// Seed that wants nothing.
    pub upload_only: bool,
    /// Does peer exchange.
    pub has_pex: bool,
}

/// `debug_peers` result.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DebugPeersDto {
    /// Transfer id.
    pub id: String,
    /// Name.
    pub name: String,
    /// State name.
    pub state: String,
    /// The peers.
    pub peers: Vec<DebugPeerDto>,
    /// More peers exist than are shown.
    pub truncated: u32,
    /// Connected over the whole client.
    pub connected: u32,
    /// Handshaking over the whole client.
    pub half_open: u32,
    /// Connection cap.
    pub max_connections: u32,
    /// Incoming connections accepted since start.
    pub incoming_accepted: u64,
    /// Incoming connections refused since start (budget, unknown torrent, bad handshake).
    pub incoming_refused: u64,
}

/// One torrent line in `debug perf`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DebugPerfTorrent {
    /// Transfer id.
    pub id: String,
    /// Name.
    pub name: String,
    /// State name.
    pub state: String,
    /// Bytes/s down.
    pub down_rate: u64,
    /// Bytes/s up.
    pub up_rate: u64,
    /// Peers connected.
    pub peers: u32,
    /// Seeds among them.
    pub seeds: u32,
    /// Bytes still to download.
    pub left_bytes: u64,
    /// Queue position.
    pub queue_pos: u32,
    /// Why it is not moving.
    #[serde(default)]
    pub waiting_note: Option<String>,
    /// Downloading but practically nothing arrives.
    pub stalled: bool,
}

/// `debug_perf` result.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DebugPerfDto {
    /// Rates and limits.
    pub totals: TotalsDto,
    /// Limits in force.
    pub limits: LimitsDto,
    /// Gates, budgets, DHT, port mapping.
    pub probe: ProbeDto,
    /// The server process.
    pub process: ProcessDto,
    /// Active torrents (downloading, checking, stalled first), at most 25.
    pub torrents: Vec<DebugPerfTorrent>,
    /// Torrents in total.
    pub torrent_count: u32,
    /// Peers connected over all torrents.
    pub peers: u32,
}

/// One finding of `debug health`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct HealthIssue {
    /// `offline`, `port`, `tracker`, `disk`, `stalled`, `limits`, `memory`.
    pub area: String,
    /// `problem` or `notice`.
    pub severity: String,
    /// What is wrong, in words.
    pub message: String,
    /// The command that digs deeper.
    pub next: String,
}

/// `debug_health` result.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DebugHealthDto {
    /// Nothing wrong.
    pub ok: bool,
    /// The findings.
    pub issues: Vec<HealthIssue>,
    /// One line.
    pub summary: String,
}

// ----------------------------------------------------------------------------- folder sync (SPEC 7G)

/// A path that cannot be synced, with the reason.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SyncIssueDto {
    /// The path inside the folder.
    pub path: String,
    /// Why, in words.
    pub reason: String,
}

/// One synced folder (`sync.list`, `sync.status`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SyncFolderDto {
    /// The folder id (`MFZWI3D-BONSGYC-…`); its first group identifies it in commands.
    pub id: String,
    /// Its name on this Mac.
    pub name: String,
    /// Where it is.
    pub path: String,
    /// What this Mac may do: `read_write` or `read_only` (from its role; `read_only` while joining).
    pub access: String,
    /// This Mac's role: `owner`, `admin`, `read_write` or `read_only`; absent while joining or after removal.
    #[serde(default)]
    pub role: Option<String>,
    /// When the folder stops being shared (Unix seconds; its owners' setting): after it only the owners sync.
    /// Absent: no end.
    #[serde(default)]
    pub share_until: Option<u64>,
    /// `starting`, `up-to-date`, `scanning`, `syncing`, `waiting-for-devices`, `waiting-for-admin`, `paused`,
    /// `missing`, `error`.
    pub state: String,
    /// Files and folders in it (as last scanned).
    pub files: u64,
    /// Their size.
    pub bytes: u64,
    /// Paths still to fetch.
    pub need_files: u64,
    /// Bytes still to fetch.
    pub need_bytes: u64,
    /// Bytes per second fetched.
    pub rate_in: u64,
    /// Bytes per second served.
    pub rate_out: u64,
    /// Devices connected for this folder.
    pub connected: u32,
    /// Devices known in this folder.
    pub members: u32,
    /// Conflict copies not yet resolved.
    pub conflicts: u32,
    /// Only synced with devices on the local network.
    #[serde(default)]
    pub lan_only: bool,
    /// Paths that cannot be synced.
    #[serde(default)]
    pub issues: Vec<SyncIssueDto>,
    /// The last error, when the state is `error`.
    #[serde(default)]
    pub error: Option<String>,
    /// When the folder last had nothing left to do (Unix seconds).
    #[serde(default)]
    pub last_synced_at: Option<u64>,
}

/// One unresolved conflict copy (`sync.conflicts`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SyncConflictDto {
    /// The copy, relative to the folder (`/`-separated).
    pub copy: String,
    /// The path it is a copy of.
    pub original: String,
    /// The device that made the losing version (empty when the name does not say).
    pub device: String,
    /// When, as the copy's name says it (`2026-10-25 14.32`; empty when it does not).
    pub when: String,
}

/// `sync.conflicts` result: the unresolved conflict copies of a folder.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SyncConflictsDto {
    /// The folder id.
    pub id: String,
    /// Its name.
    pub name: String,
    /// The copies.
    pub conflicts: Vec<SyncConflictDto>,
}

/// A connected device.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SyncDeviceDto {
    /// Its device id.
    pub id: String,
    /// Its name.
    pub name: String,
    /// Its client (`Seedwell 0.1.5`).
    pub client: String,
    /// Where the connection goes.
    pub address: String,
    /// `LAN`, `Internet` or `Relay`.
    pub via: String,
    /// The folders shared with it (ids).
    pub folders: Vec<String>,
    /// Bytes received from it in this session.
    pub rx_bytes: u64,
    /// Bytes sent to it.
    pub tx_bytes: u64,
}

/// `sync.list` result.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SyncListDto {
    /// This device's id.
    pub device_id: String,
    /// This device's name.
    pub device_name: String,
    /// The TCP port sync listens on.
    pub port: u16,
    /// The port as mapped on the router, when port mapping is on and the router answered.
    #[serde(default)]
    pub external_port: Option<u16>,
    /// The folders.
    pub folders: Vec<SyncFolderDto>,
    /// The connected devices.
    pub devices: Vec<SyncDeviceDto>,
}

/// One invitation of a folder (`sync.share`, `sync.invites`). The link is a secret: anyone who has it can join
/// until it is used up, expires or is revoked.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SyncInviteDto {
    /// Its id (hex), for `sync.revoke_invite`.
    pub id: String,
    /// The `seedwell://sync/…` link (empty for one another device made).
    pub link: String,
    /// The role it admits at: `owner`, `admin`, `read_write` or `read_only`.
    pub role: String,
    /// Devices it admits; 0 = unlimited.
    pub max_uses: u32,
    /// Devices admitted with it.
    pub uses: u32,
    /// When it stops admitting (Unix seconds); absent: never.
    #[serde(default)]
    pub expires_at: Option<u64>,
    /// When it was made.
    pub created_at: u64,
    /// Revoked.
    pub revoked: bool,
}

/// `sync.invites` result.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SyncInvitesDto {
    /// The folder id.
    pub id: String,
    /// Its name.
    pub name: String,
    /// Newest first.
    pub invites: Vec<SyncInviteDto>,
}

/// One device of a folder (`sync.members`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SyncMemberDto {
    /// Its device id.
    pub device_id: String,
    /// Its name (empty when never met).
    pub name: String,
    /// `owner`, `admin`, `read_write` or `read_only`.
    pub role: String,
    /// Connected now for this folder.
    pub online: bool,
    /// This Mac.
    pub me: bool,
    /// When its access ends (Unix seconds); absent: until removed.
    #[serde(default)]
    pub expires_at: Option<u64>,
}

/// `sync.members` result.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SyncMembersDto {
    /// The folder id.
    pub id: String,
    /// Its name.
    pub name: String,
    /// This Mac first, then by role.
    pub members: Vec<SyncMemberDto>,
}

/// `sync.settings` and `sync.set_settings` result.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SyncSettingsDto {
    /// The name other devices see (empty = the computer's name).
    pub device_name: String,
    /// The configured sync port (0 = chosen at first start).
    pub port: u16,
    /// Find devices on the local network.
    pub lan_discovery: bool,
    /// Find devices over the Internet.
    pub dht_discovery: bool,
    /// Relay between this user's own devices.
    pub relay_own_devices: bool,
    /// Also relay between other members of shared folders.
    pub relay_everyone: bool,
    /// Internet upload limit, bytes per second (`None` = unlimited).
    #[serde(default)]
    pub up_limit: Option<u64>,
    /// Internet download limit, bytes per second (`None` = unlimited).
    #[serde(default)]
    pub down_limit: Option<u64>,
    /// Where joined folders go by default (empty = `~/Seedwell`).
    pub default_location: String,
    /// The settings that were changed by this call.
    #[serde(default)]
    pub changed: Vec<String>,
    /// What to know (a port change applies at the next start).
    #[serde(default)]
    pub note: String,
}

/// `sync.remove` result (step 1).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SyncRemovePlanDto {
    /// Pass this to `sync.remove_confirm`.
    pub token: String,
    /// Seconds the token stays valid.
    pub expires_in_secs: u64,
    /// Whether the folder goes to the Trash.
    pub delete_files: bool,
    /// The folder.
    pub folder: SyncFolderDto,
    /// Plain-language summary to show a person.
    pub summary: String,
}

/// `sync.remove_confirm` result.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SyncRemovedDto {
    /// The folder id.
    pub id: String,
    /// Its name.
    pub name: String,
    /// Where it was.
    pub path: String,
    /// `kept` (the files stay where they are) or `trashed`.
    pub files: String,
    /// Where in the Trash it went.
    #[serde(default)]
    pub trashed_to: Option<String>,
}
