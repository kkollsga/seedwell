//! trs-sync-proto: Seedwell Sync v1, everything two devices must agree on (SPEC §7G).
//!
//! * [`model`]: version vectors and the file model (Syncthing's rules).
//! * [`ids`]: device and folder identities and their checked text form.
//! * [`keys`]: folder keys, their derivation, links and the access proofs.
//! * [`device`]: a device's Ed25519 key.
//! * [`sign`]: signed file versions and the writer table.
//! * [`codec`] and [`msg`]: framing and the wire messages.
//! * [`base32`] and [`random`]: helpers.
//!
//! Sans-I/O: no sockets, no files, no clocks. See the README for the derivations and the wire format.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod base32;
pub mod codec;
pub mod device;
pub mod ids;
pub mod keys;
pub mod model;
pub mod msg;
pub mod random;
pub mod sign;

pub use crate::device::DeviceKey;
pub use crate::ids::{DeviceId, FolderId, ShortId};
pub use crate::keys::{Access, FolderKey, FolderSecret, Link, Proof};
pub use crate::msg::Message;
pub use crate::random::Random;
