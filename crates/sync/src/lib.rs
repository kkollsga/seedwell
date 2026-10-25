//! trs-sync: the folder-sync core (SPEC §7G).
//!
//! * [`fs`]: the injected filesystem ([`fs::StdFs`] on disk, [`fs::MemFs`] in simulations).
//! * [`path`]: wire names (NFC) and comparison keys (case folding on case-insensitive folders).
//! * [`ignore`]: the default ignores and the `.seedwellignore` matcher.
//! * [`chunk`]: FastCDC chunking with SHA-256, size tiers and their hysteresis.
//! * [`global`]: which version of a path is current, and what this device must do about it.
//! * [`index`]: local and remote records and the derived need list ([`index::MemStore`] in memory).
//! * [`store_redb`]: [`store_redb::RedbStore`], the index in one redb file per folder.
//! * [`scan`], [`plan`], [`pull`]: disk → records, needs → steps, steps → disk.
//! * [`conflict`], [`archive`]: conflict copy names, the archive and its retention.
//! * [`folder`]: [`folder::FolderCore`], one folder with all of the above.
//! * [`clock`]: injected time.
//!
//! No sockets and no async: the driver (`trs-sync-net`) runs this on blocking threads and moves
//! messages. See the README for the design.
// `unsafe` only in `fs::flush`, the one OS call `std` lacks (a write barrier).
#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod archive;
pub mod chunk;
pub mod clock;
pub mod conflict;
pub mod folder;
pub mod fs;
pub mod global;
pub mod ignore;
pub mod index;
pub mod path;
pub mod plan;
pub mod pull;
pub mod scan;
pub mod store_redb;
