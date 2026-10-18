#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod classifier;
mod display;
mod game;
mod hint;
mod nameinfo;
mod names;
mod profile;
mod structure;
mod tags;
mod titlecase;
mod tokens;
mod types;

pub use classifier::classify;
pub use display::{clean_name, display_name, display_title, torrent_title};
pub use hint::category_from_hint;
pub use names::{collision_suffix, unique_path};
pub use structure::{
    Cpu, Group, GroupKind, Item, MAX_FILES, Os, PkgKind, Place, Structure, StructureKind, Variant,
    library_identities, structure,
};
pub use types::{Category, Classification, Fields, FileEntry, Input, JunkFile, JunkKind};
