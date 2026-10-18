/// Something odd but survivable that parsing noticed. Kept as data (not logged)
/// so the UI or CLI can decide whether to show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Warning {
    /// A tracker URL was dropped: unsupported scheme, not UTF-8, or empty host.
    IgnoredTracker(Box<str>),
    /// A web-seed URL was dropped (only http/https are usable).
    IgnoredWebSeed(Box<str>),
    /// More trackers or web seeds than the caps; the rest were dropped.
    TooManyUrls,
    /// Count of path components that contained invalid UTF-8 and were repaired.
    LossyPathComponents(usize),
    /// Count of path components in which `:` was replaced by `_`.
    ColonMappedComponents(usize),
    /// Count of BEP 47 symlink entries. They are listed but must never be
    /// materialised as symlinks (a symlink can point anywhere on disk).
    SymlinkEntries(usize),
    /// v2/hybrid torrent: per-file merkle roots and `piece layers` are not
    /// parsed yet, so v2 piece verification is unavailable.
    V2PieceLayersIgnored,
}
