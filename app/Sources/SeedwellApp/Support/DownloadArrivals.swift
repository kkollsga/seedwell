import Foundation

/// The app-wide watcher: one `Arrivals` (the core's: which torrents just became downloads, SPEC 8.2, 8.3), fed by
/// the menu bar, the window and the events.
@MainActor final class DownloadWatcher {
    let arrivals = Arrivals()
    /// Called with each new download, oldest first, after the arrivals are recorded.
    var onArrival: (String) -> Void = { _ in }

    func note(_ s: Snapshot) {
        for id in arrivals.observe(snapshot: s, nowUnixMs: Date().unixMs) { onArrival(id) }
    }

    /// The user changed the wanted files of a torrent (a finished one may become a download again).
    func userReselected(_ id: String) {
        if arrivals.userReselected(id: id, nowUnixMs: Date().unixMs) { onArrival(id) }
    }
}
