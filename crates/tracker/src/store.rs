//! Persisted per-(torrent, tracker) timing (SPEC 7C.2).
//!
//! The engine implements [`TimingStore`] on its transactional store (redb).
//! The core only ever asks for a record when a machine is created and emits
//! `Persist` intents when something worth surviving a crash changes.

use std::collections::HashMap;

use crate::time::Millis;
use crate::types::InfoHash20;

/// Key of one record.
///
/// `Debug` is redacted: it prints the scheme, host and port of the tracker but
/// never the path or query, which carry the passkey (SPEC 7B.4).
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct TimingKey {
    /// Torrent.
    pub info_hash: InfoHash20,
    /// Raw tracker URL (contains the passkey: never log).
    pub tracker_url: String,
}

impl std::fmt::Debug for TimingKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TimingKey { info_hash: ")?;
        for b in &self.info_hash[..4] {
            write!(f, "{b:02x}")?;
        }
        f.write_str("…, tracker_url: ")?;
        match crate::url::TrackerUrl::parse(&self.tracker_url) {
            Ok(u) => write!(f, "{u}")?,
            Err(_) => f.write_str("<unparseable>")?,
        }
        f.write_str(" }")
    }
}

/// What survives a restart.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct TimingRecord {
    /// Time of the last announce we actually sent.
    pub last_announce_at: Option<Millis>,
    /// Earliest time a regular (non-event) announce may be sent.
    pub next_allowed_at: Option<Millis>,
    /// Earliest time a manual re-announce (or a `started` after an unclean
    /// shutdown) may be sent: last success + min interval.
    pub min_allowed_at: Option<Millis>,
    /// `completed` has been dispatched to this tracker, ever. Written *before*
    /// the request leaves, so a crash can never cause a second `completed`.
    pub completed_sent: bool,
    /// The torrent completed but `completed` has not been dispatched yet.
    pub completion_owed: bool,
    /// Obsolete: the `tracker id` is per session only (like Transmission and
    /// libtorrent) and is no longer persisted or restored. The field is kept
    /// so existing store implementations keep compiling; it is always written
    /// as `None` and ignored on load.
    pub tracker_id: Option<Box<[u8]>>,
    /// A session was open (started sent, stopped not confirmed): the previous
    /// run ended without a clean `stopped`.
    pub session_open: bool,
}

/// Error from the backing store.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct StoreError(pub String);

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "timing store error: {}", self.0)
    }
}
impl std::error::Error for StoreError {}

/// Durable storage for [`TimingRecord`]s. `save` must be durable when it
/// returns `Ok` (the caller releases a request right after it).
pub trait TimingStore {
    /// Load a record, if any.
    fn load(&self, key: &TimingKey) -> Option<TimingRecord>;
    /// Replace a record durably.
    fn save(&mut self, key: &TimingKey, record: &TimingRecord) -> Result<(), StoreError>;
}

/// In-memory store for tests and simulations. `fail_saves` injects failures.
/// `Debug` prints only the record count (the keys contain passkeys).
#[derive(Default, Clone)]
pub struct MemoryStore {
    map: HashMap<TimingKey, TimingRecord>,
    /// When true every `save` fails (fault injection).
    pub fail_saves: bool,
}

impl std::fmt::Debug for MemoryStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryStore")
            .field("records", &self.map.len())
            .field("fail_saves", &self.fail_saves)
            .finish()
    }
}

impl MemoryStore {
    /// Empty store.
    pub fn new() -> Self {
        Self::default()
    }
    /// Number of stored records.
    pub fn len(&self) -> usize {
        self.map.len()
    }
    /// True when empty.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

impl TimingStore for MemoryStore {
    fn load(&self, key: &TimingKey) -> Option<TimingRecord> {
        self.map.get(key).cloned()
    }
    fn save(&mut self, key: &TimingKey, record: &TimingRecord) -> Result<(), StoreError> {
        if self.fail_saves {
            return Err(StoreError("injected failure".into()));
        }
        match self.map.get_mut(key) {
            Some(slot) => slot.clone_from(record),
            None => {
                self.map.insert(key.clone(), record.clone());
            }
        }
        Ok(())
    }
}
