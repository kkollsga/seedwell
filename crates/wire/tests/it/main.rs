//! All of this crate's integration tests in one binary (one link, one launch): each former
//! `tests/<name>.rs` is a module here. Tests that measure the process (memory) keep their own binary.


mod flow;
mod network;
mod proptests;
