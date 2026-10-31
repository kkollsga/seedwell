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
//! * [`ondemand`]: on-demand folders' items for a host (File Provider, Cloud Files), and their change feed.
//! * [`clock`]: injected time.
//!
//! No sockets and no async: the driver (`trs-sync-net`) runs this on blocking threads and moves
//! messages. See the README for the design.
// `unsafe` only in `fs::flush`, the one OS call `std` lacks (a write barrier).
#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod archive;

/// The folder agents pass files through (SPEC §7G.10): a file there is a copy in transit whose original stays with
/// its sender, so one deleted there (received, or cleaned up) is not archived.
pub const PASS_THROUGH: &str = "Seedwell Pass-Through";

/// `path` (wire form) is in the pass-through folder.
pub fn in_pass_through(path: &str) -> bool {
    path.strip_prefix(PASS_THROUGH).is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}
pub mod chunk;
pub mod clock;
pub mod conflict;
pub mod folder;
pub mod fs;
pub mod global;
pub mod ignore;
pub mod index;
pub mod ondemand;
pub mod path;
pub mod plan;
pub mod pull;
pub mod scan;
pub mod store_redb;
