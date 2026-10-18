#![forbid(unsafe_code)]
#![warn(missing_docs)]
//! Persistent state for Seedwell. See `README.md`.

mod codec;
pub mod config;
pub mod doc;
pub mod duration;
pub mod error;
pub mod fsutil;
pub mod ids;
mod legacy;
pub mod record;
pub mod rules;
mod serde_util;
pub mod stats;
pub mod store;
#[cfg(test)]
mod testutil;
pub mod timing;
pub mod trackers;
pub mod url;
pub mod volume;
pub mod xref;

pub use config::Config;
pub use doc::{DocStore, Document, LoadSource, Loaded, Parsed, PollEvent, Poller, WatchHandle, spawn_watcher};
pub use error::{DecodeError, DocError, FieldError, StateError, Warning};
pub use record::{Counters, ResumeData, TorrentFlags, TorrentRecord};
pub use rules::RuleSet;
pub use stats::{Day, StatsBucket, StatsScope, StatsSummary};
pub use store::{InfoHash, Opened, OpenOptions, OpenReport, Store};
pub use timing::TimingHandle;
pub use trackers::TrackerProfiles;
pub use xref::cross_check;

#[cfg(test)]
mod tests {
    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn handles_can_cross_threads() {
        assert_send_sync::<crate::Store>();
        assert_send_sync::<crate::TimingHandle>();
        assert_send_sync::<crate::Config>();
        assert_send_sync::<crate::RuleSet>();
        assert_send_sync::<crate::TrackerProfiles>();
        assert_send_sync::<crate::StateError>();
    }
}
