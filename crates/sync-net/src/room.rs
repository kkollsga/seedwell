//! Where a message's attachments belong (SPEC §7G.7 *Attachments*): a channel, or a private conversation. Each is a
//! [`Room`]: its images ([`crate::blobs`]) and files ([`crate::files`]) are kept in its own container, fetched only
//! from the devices it may be fetched from, and deleted with it.
//!
//! * A channel's attachments are fetched from any connected member of the channel ([`BlobScope::Channel`]).
//! * A private conversation's only from its other device, over a direct session between the two
//!   ([`BlobScope::Direct`]): never through another device, as its messages.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use trs_sync_proto::attach::{AttachmentRef, Attachments};
use trs_sync_proto::ids::{DeviceId, FolderId};
use trs_sync_proto::msg::BlobScope;

use crate::blobs::Blobs;
use crate::files::Files;
use crate::shared::{Ctx, PeerHandle};

/// A channel or a private conversation, as callers name it: the channel's id, or `dm:<device id>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Place {
    /// A channel.
    Channel(FolderId),
    /// The private conversation with a device.
    Direct(DeviceId),
}

impl fmt::Display for Place {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Place::Channel(id) => write!(f, "{id}"),
            Place::Direct(d) => write!(f, "dm:{d}"),
        }
    }
}

impl FromStr for Place {
    type Err = trs_sync_proto::ids::IdError;
    fn from_str(s: &str) -> Result<Place, Self::Err> {
        match s.trim().strip_prefix("dm:") {
            Some(d) => d.parse().map(Place::Direct),
            None => s.trim().parse().map(Place::Channel),
        }
    }
}

impl From<FolderId> for Place {
    fn from(id: FolderId) -> Place {
        Place::Channel(id)
    }
}

impl From<&FolderId> for Place {
    fn from(id: &FolderId) -> Place {
        Place::Channel(*id)
    }
}

/// An attachment a kept message names, and the device that wrote that message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Named {
    /// Without its preview.
    pub(crate) r: AttachmentRef,
    /// The author's public key.
    pub(crate) author: [u8; 32],
}

/// Every attachment the kept messages of a room name.
#[derive(Default)]
pub(crate) struct Index {
    /// Images and files by their hash (the newest message's record, without its preview).
    pub(crate) refs: HashMap<[u8; 32], Named>,
    /// A file's piece table's hash → the file's hash.
    pub(crate) tables: HashMap<[u8; 32], [u8; 32]>,
    /// The newest images within the room's budget, newest first: fetched by themselves.
    pub(crate) budget: Vec<[u8; 32]>,
    pub(crate) budget_set: HashSet<[u8; 32]>,
}

impl Index {
    /// The index of messages' attachments, newest message first, each with its author; images are fetched by
    /// themselves up to `budget` bytes.
    pub(crate) fn build(newest_first: impl Iterator<Item = (Attachments, [u8; 32])>, budget: u64) -> Index {
        let mut ix = Index::default();
        let mut total = 0u64;
        let mut full = false;
        for (a, author) in newest_first {
            for r in a.refs {
                if ix.refs.contains_key(&r.hash) {
                    continue;
                }
                if r.is_file() {
                    ix.tables.insert(r.pieces, r.hash);
                } else if !full {
                    if total.saturating_add(r.size) <= budget {
                        total += r.size;
                        ix.budget.push(r.hash);
                        ix.budget_set.insert(r.hash);
                    } else {
                        full = true;
                    }
                }
                ix.refs.insert(r.hash, Named { r: AttachmentRef { thumb: Vec::new(), ..r }, author });
            }
        }
        ix
    }

    /// The images the room wants that are not failed: (hash, kind, size), in the budget's order and then `pinned`.
    pub(crate) fn wanted_images(&self, pinned: &HashSet<[u8; 32]>) -> Vec<([u8; 32], trs_media::Kind, u64)> {
        let mut seen = HashSet::new();
        self.budget
            .iter()
            .chain(pinned.iter())
            .filter(|h| seen.insert(**h))
            .filter_map(|h| {
                let n = self.refs.get(h)?;
                let k = trs_media::Kind::from_byte(n.r.kind)?;
                (n.r.size > 0 && n.r.size <= k.max_bytes() as u64).then_some((*h, k, n.r.size))
            })
            .collect()
    }
}

/// A channel or a private conversation, for fetching and serving its attachments.
pub(crate) trait Room: Send + Sync + 'static {
    /// What a [`trs_sync_proto::msg::BlobWant`] for it says.
    fn scope(&self) -> BlobScope;
    /// The connected devices its attachments may be fetched from (with the blobs bit).
    fn peers(&self, ctx: &Ctx) -> Vec<Arc<PeerHandle>>;
    /// Its images on this device.
    fn blobs(&self) -> &Blobs;
    /// Its files on this device.
    fn files(&self) -> &Files;
    /// Runs `f` on its attachments' index.
    fn with_index(&self, ctx: &Ctx, f: &mut dyn FnMut(&Index));
    /// Something went wrong that the user should see.
    fn problem(&self, why: String);
}

/// An attachment of a room by its hash.
pub(crate) fn named(ctx: &Ctx, room: &dyn Room, hash: &[u8; 32]) -> Option<Named> {
    let mut out = None;
    room.with_index(ctx, &mut |ix| out = ix.refs.get(hash).cloned());
    out
}

/// The file whose piece table has this hash.
pub(crate) fn file_of_table(ctx: &Ctx, room: &dyn Room, table: &[u8; 32]) -> Option<Named> {
    let mut out = None;
    room.with_index(ctx, &mut |ix| out = ix.tables.get(table).and_then(|h| ix.refs.get(h)).cloned());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_place_is_a_channel_id_or_dm_and_a_device() {
        let ch = Place::Channel(FolderId([7; 32]));
        let dm = Place::Direct(DeviceId([8; 32]));
        assert_eq!(ch.to_string().parse::<Place>().unwrap(), ch);
        assert_eq!(dm.to_string().parse::<Place>().unwrap(), dm);
        assert!(dm.to_string().starts_with("dm:"));
        assert!("dm:nonsense".parse::<Place>().is_err());
    }
}
