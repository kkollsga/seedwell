import Foundation

/// Which torrents just became downloads (SPEC 8.2, 8.3). A pure value: feed it every snapshot,
/// it answers the ids that are new since the last one.
///
/// A new download is a torrent that is not held (no countdown, no file choice), is going to download
/// (downloading, fetching metadata, or waiting in the queue) and was not seen as a download before.
/// A torrent held at first sight becomes an arrival when it is released (Start Now, OK, the countdown
/// running out); one cancelled while held never does. The first snapshot is the baseline: whatever
/// was downloading when the app started is not new. Paused, checking or failed rows are not arrivals
/// until they actually download.
///
/// A torrent that was shared before (it seeded for a while) and downloads again is an arrival only when
/// the user re-selected files for it (`reselected`, within a minute). One a recheck at launch found
/// pieces missing in comes back as a download quietly: nothing takes the window.
struct DownloadArrivals {
    /// How long a file re-selection can explain a torrent turning into a download.
    static let reselectWindow: TimeInterval = 60

    /// Order of first sight, for the menu (newest first). Rows of the baseline share rank 0.
    private(set) var rank: [String: Int] = [:]
    /// Ids that already were arrivals (or were there at the start); they never switch again.
    private var started: Set<String> = []
    /// Arrivals of this run, oldest first, while they are still downloads or queued.
    private(set) var arrived: [String] = []
    private var primed = false
    private var sequence = 0
    /// Torrents the user just changed the wanted files of, with the time.
    private var reselections: [String: Date] = [:]
    /// Shared-before torrents that turned into downloads without the user asking (not arrivals, so far).
    private var quiet: Set<String> = []

    /// The rows that can arrive: downloads that are going to move data.
    static func isEligible(_ r: TransferRow) -> Bool {
        guard !r.isHeld else { return false }
        switch r.state {
        case .downloading, .metadata, .queued: return true
        default: return false
        }
    }

    /// A torrent that finished and seeded before: its next download is not a fresh add.
    static func wasShared(_ r: TransferRow) -> Bool { r.kind == .torrent && r.seedSecs > 0 }

    /// The user changed which files of `id` are wanted (ticked, Files…, the chooser). Returns true when
    /// the torrent already turned into a download a moment ago, so it is an arrival after all.
    mutating func userReselected(_ id: String, now: Date = Date()) -> Bool {
        if quiet.remove(id) != nil {
            arrived.append(id)
            return true
        }
        reselections[id] = now
        return false
    }

    private mutating func isReselected(_ id: String, now: Date) -> Bool {
        reselections = reselections.filter { now.timeIntervalSince($0.value) <= Self.reselectWindow }
        return reselections.removeValue(forKey: id) != nil
    }

    /// Returns the new downloads of this snapshot, oldest first.
    mutating func observe(_ s: Snapshot, now: Date = Date()) -> [String] {
        let rows = s.downloading + s.queued
        let present = Set(rows.map(\.id))
        if !primed {
            primed = true
            started = present
            for r in rows { rank[r.id] = 0 }
            return []
        }
        // Gone (finished, removed): adding it again later is new again.
        started.formIntersection(present)
        rank = rank.filter { present.contains($0.key) }
        arrived.removeAll { !present.contains($0) }
        quiet.formIntersection(present)
        var fresh: [String] = []
        for r in rows {
            if rank[r.id] == nil { sequence += 1; rank[r.id] = sequence }
            if Self.isEligible(r), started.insert(r.id).inserted {
                if Self.wasShared(r), !isReselected(r.id, now: now) {
                    quiet.insert(r.id)
                    continue
                }
                fresh.append(r.id)
                arrived.append(r.id)
            }
        }
        return fresh
    }

    func rank(of id: String) -> Int { rank[id] ?? 0 }
}

/// The app-wide watcher: one `DownloadArrivals`, fed by the menu bar, the window and the events.
@MainActor final class DownloadWatcher {
    private(set) var arrivals = DownloadArrivals()
    /// Called with each new download, oldest first, after the arrivals are recorded.
    var onArrival: (String) -> Void = { _ in }

    func note(_ s: Snapshot) {
        for id in arrivals.observe(s) { onArrival(id) }
    }

    /// The user changed the wanted files of a torrent (a finished one may become a download again).
    func userReselected(_ id: String) {
        if arrivals.userReselected(id) { onArrival(id) }
    }
}

/// What a closed window remembers: the newest download that arrived while it was away. The next opening
/// goes to it if it is still downloading, and forgets it either way.
struct ClosedWindowArrival: Equatable {
    private(set) var id: String?

    mutating func remember(_ id: String) { self.id = id }

    /// The id to select on opening, if it still is a download.
    mutating func take(stillDownloading: (String) -> Bool) -> String? {
        defer { id = nil }
        return id.flatMap { stillDownloading($0) ? $0 : nil }
    }
}
