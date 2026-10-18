import Foundation

/// Notifications and hidden mode (SPEC 7E.7). A banner names a torrent, which defeats hiding it: for a
/// tracker in full hidden mode nothing about its torrents is announced; in "hide only what is sharing"
/// mode downloads are announced as usual and anything about seeding is not.
enum HiddenNotices {
    enum Kind {
        /// A download starting, needing a choice, finishing or failing.
        case download
        /// A torrent that is seeding: its goal, or an error while sharing.
        case sharing
    }

    /// Whether a notice of `kind` about a torrent of `trackerID` is held back.
    static func silenced(_ trackerID: String?, _ kind: Kind, in trackers: [TrackerSummary]) -> Bool {
        guard let id = trackerID, let t = trackers.first(where: { $0.id == id }), t.hidden else { return false }
        return kind == .sharing || !t.hideOnlySeeding
    }
}
