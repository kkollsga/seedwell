//! All of this crate's integration tests in one binary (one link, one launch): each former
//! `tests/<name>.rs` is a module here. Tests that measure the process (memory) keep their own binary.


mod corpus;
mod display_corpus;
mod props;
mod rules;
mod structure;
mod structure_corpus;
mod structure_props;
