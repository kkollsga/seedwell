//! Durable tracker timing: the [`TimingStore`] implementation over `state.redb` (SPEC §7C.2).
//!
//! `save` commits its own transaction before returning, so when the tracker sees `Ok` the
//! `completed_sent` flag is on disk and a crash can never lead to a second `completed` announce.
//! It does not go through the 2 s debounce.
//!
//! Failure policy: `load` cannot return an error through the trait. If the row exists but cannot
//! be read or decoded, the safe answer is returned instead of "no record": `completed_sent = true`
//! and no pending completion, so the worst outcome is a missed `completed` event, never a
//! duplicate one. Absence of a row (never written) is the only case that yields `None`.

use std::sync::Weak;

use redb::ReadableDatabase;
use trs_tracker::store::{StoreError, TimingKey, TimingRecord, TimingStore};
use trs_tracker::time::Millis;

use crate::codec::{Reader, Writer};
use crate::error::DecodeError;
use crate::store::{Shared, Store, TIMING};

const TIMING_V: u8 = 1;
const MAX_URL: usize = 2048;
const MAX_TRACKER_ID: usize = 256;

/// Cloneable, thread-safe handle implementing [`TimingStore`]. It does not keep the database open:
/// after the [`Store`] is dropped every call fails with "store closed".
#[derive(Clone)]
pub struct TimingHandle {
    shared: Weak<Shared>,
}

impl Store {
    /// A handle for the tracker crate's timing persistence.
    pub fn timing_handle(&self) -> TimingHandle {
        TimingHandle { shared: std::sync::Arc::downgrade(&self.shared) }
    }
}

fn key_bytes(key: &TimingKey) -> Result<Vec<u8>, StoreError> {
    if key.tracker_url.len() > MAX_URL {
        return Err(StoreError("tracker URL longer than 2048 bytes".into()));
    }
    let mut k = Vec::with_capacity(20 + key.tracker_url.len());
    k.extend_from_slice(&key.info_hash);
    k.extend_from_slice(key.tracker_url.as_bytes());
    Ok(k)
}

pub(crate) fn encode(r: &TimingRecord) -> Vec<u8> {
    let mut w = Writer::new();
    w.u8(TIMING_V);
    let tid = r.tracker_id.as_deref().filter(|t| t.len() <= MAX_TRACKER_ID);
    let flags = u8::from(r.completed_sent)
        | u8::from(r.completion_owed) << 1
        | u8::from(r.session_open) << 2
        | u8::from(r.last_announce_at.is_some()) << 3
        | u8::from(r.next_allowed_at.is_some()) << 4
        | u8::from(r.min_allowed_at.is_some()) << 5
        | u8::from(tid.is_some()) << 6;
    w.u8(flags);
    for t in [r.last_announce_at, r.next_allowed_at, r.min_allowed_at].into_iter().flatten() {
        w.varint(t.0);
    }
    if let Some(t) = tid {
        w.bytes(t);
    }
    w.0
}

pub(crate) fn decode(b: &[u8]) -> Result<TimingRecord, DecodeError> {
    let mut r = Reader::new(b);
    if r.u8()? != TIMING_V {
        return Err(DecodeError("unknown timing version"));
    }
    let f = r.u8()?;
    let mut t = |bit: u8| -> Result<Option<Millis>, DecodeError> {
        if f & bit != 0 { Ok(Some(Millis(r.varint()?))) } else { Ok(None) }
    };
    let (last_announce_at, next_allowed_at, min_allowed_at) = (t(8)?, t(16)?, t(32)?);
    let tracker_id = if f & 64 != 0 { Some(r.bytes(MAX_TRACKER_ID)?.to_vec().into_boxed_slice()) } else { None };
    r.finish()?;
    Ok(TimingRecord {
        last_announce_at,
        next_allowed_at,
        min_allowed_at,
        completed_sent: f & 1 != 0,
        completion_owed: f & 2 != 0,
        tracker_id,
        session_open: f & 4 != 0,
    })
}

impl TimingHandle {
    /// Fallible load: `Ok(None)` only when there is no row.
    pub fn try_load(&self, key: &TimingKey) -> Result<Option<TimingRecord>, StoreError> {
        let shared = self.shared.upgrade().ok_or_else(|| StoreError("store closed".into()))?;
        let k = key_bytes(key)?;
        let rtx = shared.db.begin_read().map_err(|e| StoreError(e.to_string()))?;
        let t = rtx.open_table(TIMING).map_err(|e| StoreError(e.to_string()))?;
        let Some(v) = t.get(k.as_slice()).map_err(|e| StoreError(e.to_string()))? else { return Ok(None) };
        decode(v.value()).map(Some).map_err(|e| StoreError(e.to_string()))
    }

    /// Durable save: returns after the transaction is committed.
    pub fn save_durable(&self, key: &TimingKey, record: &TimingRecord) -> Result<(), StoreError> {
        let shared = self.shared.upgrade().ok_or_else(|| StoreError("store closed".into()))?;
        let k = key_bytes(key)?;
        let v = encode(record);
        let w = shared.db.begin_write().map_err(|e| StoreError(e.to_string()))?;
        {
            let mut t = w.open_table(TIMING).map_err(|e| StoreError(e.to_string()))?;
            t.insert(k.as_slice(), v.as_slice()).map_err(|e| StoreError(e.to_string()))?;
        }
        w.commit().map_err(|e| StoreError(e.to_string()))
    }
}

impl TimingStore for TimingHandle {
    fn load(&self, key: &TimingKey) -> Option<TimingRecord> {
        match self.try_load(key) {
            Ok(r) => r,
            Err(_) => Some(TimingRecord { completed_sent: true, ..TimingRecord::default() }),
        }
    }

    fn save(&mut self, key: &TimingKey, record: &TimingRecord) -> Result<(), StoreError> {
        self.save_durable(key, record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_codec_roundtrip() {
        let full = TimingRecord {
            last_announce_at: Some(Millis(1_791_000_000_000)),
            next_allowed_at: Some(Millis(1_791_000_900_000)),
            min_allowed_at: Some(Millis(0)),
            completed_sent: true,
            completion_owed: true,
            tracker_id: Some(b"abc".to_vec().into_boxed_slice()),
            session_open: true,
        };
        assert_eq!(decode(&encode(&full)), Ok(full));
        let empty = TimingRecord::default();
        assert_eq!(decode(&encode(&empty)), Ok(empty));
    }

    #[test]
    fn decode_rejects_garbage() {
        let good = encode(&TimingRecord { last_announce_at: Some(Millis(5)), ..Default::default() });
        for cut in 0..good.len() {
            assert!(decode(&good[..cut]).is_err());
        }
        assert!(decode(&[9, 0]).is_err());
        assert!(decode(&[1, 64, 200]).is_err());
    }
}
