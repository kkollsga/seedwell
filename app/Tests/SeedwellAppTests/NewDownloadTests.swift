import Foundation
import Testing
@testable import SeedwellApp

// A new download takes the menu (SPEC 8.2); what it does to the window is the core's (library_window_tests.rs).

private func row(_ id: String, _ state: TransferState = .downloading, held: Bool = false, choice: Bool = false,
                 seeded: UInt64 = 0) -> TransferRow {
    TransferRow(
        id: id, name: id, displayName: id, kind: .torrent, category: .movie,
        sizeBytes: 100, doneBytes: 0, downRate: 0, upRate: 0, etaSecs: nil, trackerId: "public",
        trackerName: "Public", ratio: 0, ratioGoal: nil, seedSecs: seeded, state: state, finishedAt: nil, savePath: "/x",
        fileExt: "mkv", seedGroup: nil, error: nil, retryInSecs: nil, movingPercent: nil, awaitingChoice: choice,
        pending: held ? PendingStart(startsAtMs: 0, remainingMs: 7000, paused: false) : nil,
        peers: 0, swarm: nil, knownPeers: 0, hasVideo: false, videoExt: "", fileCount: 1, lastActivity: nil,
        waitingNote: nil, waitingIsError: false, trackerHalted: false)
}

private func snap(down: [TransferRow] = [], queued: [TransferRow] = []) -> Snapshot {
    Snapshot(downRate: 0, upRate: 0, downloading: down, queued: queued, recent: [], sharingBusiest: [],
             sharing: SharingCounts(popular: 0, active: 1, dormant: 0, error: 0, dead: 0, stopped: 0), sharingTotal: 1, trackers: [],
             altSpeed: false, downLimit: nil, upLimit: nil, errorCount: 0, movingPercent: nil, attention: false, listenPort: 0, connections: 0,
             online: true, offlineSinceMs: nil)
}

extension Snapshot {
    /// Nothing downloads, one torrent shares.
    static var empty: Snapshot { snap() }
}

@MainActor @Suite("New download")
struct NewDownloadTests {
    // MARK: Which torrents count as a new download (the core's `Arrivals`; its rules are tested in menu.rs)

    @Test func theWatcherCallsBackForEachNewDownload() {
        let w = DownloadWatcher()
        var got: [String] = []
        w.onArrival = { got.append($0) }
        w.note(snap(down: [row("old")]))  // the baseline
        w.note(snap(down: [row("old"), row("new")]))
        w.note(snap(down: [row("old"), row("new")]))
        #expect(got == ["new"])
        // A finished torrent that downloads again arrives once the user's re-selection says so.
        w.note(snap(down: [row("old"), row("new"), row("t", seeded: 3600)]))
        #expect(got == ["new"])
        w.userReselected("t")
        #expect(got == ["new", "t"])
    }

    // MARK: The menu

    @Test func theMenuListsTheNewestArrivalFirst() {
        let w = DownloadWatcher()
        w.note(snap(down: [row("old1"), row("old2")]))
        let s = snap(down: [row("old1"), row("old2"), row("new")])
        w.note(s)
        let m = menuModel(snapshot: s, sync: SyncTests.snap([]), arrivals: w.arrivals, nowUnixMs: 0, updateVersion: nil)
        #expect(m.sections.map(\.title) == ["Downloading"])
        #expect(m.sections[0].rows.map(\.id) == ["new", "old1", "old2"])
    }
}
