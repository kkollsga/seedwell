import Foundation
import Testing
@testable import SeedwellApp

// A new download takes the window and the menu (SPEC 8.2, 8.3).

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

@MainActor @Suite("New download")
struct NewDownloadTests {
    // MARK: Which torrents count as a new download

    @Test func theFirstSnapshotIsTheBaseline() {
        var a = DownloadArrivals()
        #expect(a.observe(snap(down: [row("old")], queued: [row("q", .queued)])).isEmpty)
        #expect(a.observe(snap(down: [row("old")], queued: [row("q", .queued)])).isEmpty)
    }

    @Test func aTorrentAddedWithoutAHoldIsNewOnce() {
        var a = DownloadArrivals()
        _ = a.observe(snap(down: [row("old")]))
        #expect(a.observe(snap(down: [row("old"), row("new")])) == ["new"])
        #expect(a.observe(snap(down: [row("old"), row("new")])).isEmpty)  // not on every refresh
    }

    @Test func aHeldTorrentArrivesWhenReleased() {
        var a = DownloadArrivals()
        _ = a.observe(snap())
        #expect(a.observe(snap(down: [row("n", held: true)])).isEmpty)              // countdown
        #expect(a.observe(snap(down: [row("n", held: true)])).isEmpty)
        #expect(a.observe(snap(down: [row("n", .downloading)])) == ["n"])           // Start Now / expiry
    }

    @Test func aChoiceThenOKArrives() {
        var a = DownloadArrivals()
        _ = a.observe(snap())
        #expect(a.observe(snap(down: [row("c", .downloading, choice: true)])).isEmpty)
        #expect(a.observe(snap(down: [row("c", .downloading)])) == ["c"])
    }

    @Test func aCancelledHeldTorrentNeverArrives() {
        var a = DownloadArrivals()
        _ = a.observe(snap())
        _ = a.observe(snap(down: [row("n", held: true)]))
        #expect(a.observe(snap()).isEmpty)
    }

    @Test func aQueuedTorrentCountsButPausedAndCheckingDoNot() {
        var a = DownloadArrivals()
        _ = a.observe(snap())
        #expect(a.observe(snap(down: [row("chk", .checking)], queued: [row("q", .queued), row("p", .paused)])) == ["q"])
        #expect(a.observe(snap(down: [row("chk", .downloading)], queued: [row("q", .queued), row("p", .paused)])) == ["chk"])
    }

    @Test func aTorrentThatLeftAndComesBackIsNewAgain() {
        var a = DownloadArrivals()
        _ = a.observe(snap())
        _ = a.observe(snap(down: [row("x")]))
        _ = a.observe(snap())
        #expect(a.observe(snap(down: [row("x")])) == ["x"])
    }

    @Test func severalAtOnceCameOldestFirstInListOrder() {
        var a = DownloadArrivals()
        _ = a.observe(snap())
        #expect(a.observe(snap(down: [row("a"), row("b")])) == ["a", "b"])
    }

    // MARK: The window

    @Test func aNewDownloadSwitchesToDownloadsAndSelectsIt() throws {
        let m = try #require(TrackerTabTests.model())
        m.select(.sharing)
        m.selection = ["done"]
        m.infoID = "done"
        #expect(m.showNewDownload("new"))
        #expect(m.section == .downloads)
        #expect(m.selection == ["new"] && m.selectionAnchor == "new")
        #expect(m.infoID == nil)
        #expect(m.scrollRequest?.id == "new")
        #expect(!m.downloadsFlag)
    }

    @Test func aNewDownloadTakesATrackerTabAndItsInfoPageToo() throws {
        let m = try #require(TrackerTabTests.model())
        m.select(.tracker("public"))
        m.trackerInfo = true
        #expect(m.showNewDownload("new"))
        #expect(m.section == .downloads && !m.trackerInfo)
    }

    @Test func notWhileSettingsIsOpenOnlyAFlag() throws {
        let m = try #require(TrackerTabTests.model())
        m.select(.settings)
        #expect(!m.showNewDownload("new"))
        #expect(m.section == .settings)
        #expect(m.downloadsFlag)
        #expect(m.selection.isEmpty)
        m.select(.downloads)
        #expect(!m.downloadsFlag)  // going there clears it
    }

    @Test func notWhileAFieldIsBeingEdited() throws {
        let m = try #require(TrackerTabTests.model())
        m.select(.tracker("public"))
        m.trackerInfo = true
        m.isBusy = { true }
        #expect(!m.showNewDownload("new"))
        #expect(m.section == .tracker("public") && m.trackerInfo)
        #expect(m.downloadsFlag)
    }

    @Test func eachNewDownloadAsksForItsOwnScroll() throws {
        let m = try #require(TrackerTabTests.model())
        m.showNewDownload("a")
        let first = m.scrollRequest
        m.showNewDownload("a")
        #expect(m.scrollRequest != first)
        if let r = m.scrollRequest { m.scrollDone(r) }
        #expect(m.scrollRequest == nil)
    }

    // MARK: The window is closed

    @Test func aClosedWindowOpensOnTheNewDownload() {
        var memory = ClosedWindowArrival()
        memory.remember("a")
        memory.remember("b")  // the newest wins
        #expect(memory.take { _ in true } == "b")
        #expect(memory.take { _ in true } == nil)  // used up
    }

    @Test func aDownloadThatIsGoneIsNotSelected() {
        var memory = ClosedWindowArrival()
        memory.remember("a")
        #expect(memory.take { _ in false } == nil)
        #expect(memory.id == nil)
    }

    // MARK: The menu

    @Test func theNewestArrivalIsFirstInTheMenu() {
        var a = DownloadArrivals()
        _ = a.observe(snap(down: [row("old1"), row("old2")]))
        _ = a.observe(snap(down: [row("old1"), row("old2"), row("new")]))
        let l = StatusMenuController.layout(snap(down: [row("old1"), row("old2"), row("new")]), arrivals: a)
        #expect(l.downloading.map(\.id) == ["new", "old1", "old2"])
    }

    @Test func heldTorrentsStayAheadOfTheNewestArrival() {
        var a = DownloadArrivals()
        _ = a.observe(snap(down: [row("old")]))
        let s = snap(down: [row("old"), row("c", choice: true), row("n", held: true), row("new")])
        _ = a.observe(s)
        #expect(StatusMenuController.layout(s, arrivals: a).downloading.map(\.id) == ["c", "n", "new", "old"])
    }

    @Test func aQueuedArrivalIsListedUnderDownloadingNotQueued() {
        var a = DownloadArrivals()
        _ = a.observe(snap(down: [row("d1")], queued: [row("oldq", .queued)]))
        let s = snap(down: [row("d1")], queued: [row("oldq", .queued), row("newq", .queued)])
        _ = a.observe(s)
        let l = StatusMenuController.layout(s, arrivals: a)
        #expect(l.downloading.map(\.id) == ["newq", "d1"])
        #expect(l.queued.map(\.id) == ["oldq"])
    }

    @Test func withoutArrivalsTheMenuKeepsTheEnginesOrder() {
        let a = DownloadArrivals()
        let s = snap(down: [row("a"), row("b")], queued: [row("q", .queued)])
        let l = StatusMenuController.layout(s, arrivals: a)
        #expect(l.downloading.map(\.id) == ["a", "b"] && l.queued.map(\.id) == ["q"])
    }

    @Test func aQueuedRowReadsUpNext() {
        #expect(row("q", .queued).menuSubtitle == "Up next · waiting for a free slot")
    }

    // MARK: A finished torrent that downloads again (more files ticked, a recheck)

    /// "Harbor Watch — S01–S02": S01 finished and seeded 9 hours, S02 ticked: 42%, 3.1 MB/s, 4 minutes.
    private func again(_ id: String = "t") -> TransferRow {
        var r = row(id, .downloading, seeded: 9 * 3600)
        r.sizeBytes = 10_000_000_000; r.doneBytes = 4_200_000_000; r.downRate = 3_100_000; r.etaSecs = 240
        return r
    }

    @Test func aReselectedTorrentIsAnArrival() {
        var a = DownloadArrivals()
        let t0 = Date(timeIntervalSince1970: 1_000)
        _ = a.observe(snap(), now: t0)
        let early = a.userReselected("t", now: t0)                     // the user ticks S02 ...
        #expect(!early)
        #expect(a.observe(snap(down: [again()]), now: t0 + 1) == ["t"])  // ... and the row turns into a download
        #expect(a.arrived == ["t"])
    }

    @Test func aSnapshotThatWasFasterThanTheMarkStillArrives() {
        var a = DownloadArrivals()
        _ = a.observe(snap())
        #expect(a.observe(snap(down: [again()])).isEmpty)              // seen first, nobody asked yet
        let late = a.userReselected("t")                               // the mark arrives late
        #expect(late)
        #expect(a.arrived == ["t"])
        #expect(a.observe(snap(down: [again()])).isEmpty)              // and only once
    }

    @Test func aRecheckAtLaunchDoesNotTakeTheWindow() {
        var a = DownloadArrivals()
        _ = a.observe(snap())                                          // the baseline: nothing downloads
        #expect(a.observe(snap(down: [again()])).isEmpty)              // pieces were missing: quietly a download
        #expect(a.arrived.isEmpty)
        // It is still listed under Downloading in the menu.
        #expect(StatusMenuController.layout(snap(down: [again()]), arrivals: a).downloading.map(\.id) == ["t"])
    }

    @Test func aMarkOlderThanAMinuteIsForgotten() {
        var a = DownloadArrivals()
        let t0 = Date(timeIntervalSince1970: 1_000)
        _ = a.observe(snap(), now: t0)
        _ = a.userReselected("t", now: t0)
        #expect(a.observe(snap(down: [again()]), now: t0 + 120).isEmpty)
    }

    @Test func aFreshAddStillArrivesWithoutAMark() {
        var a = DownloadArrivals()
        _ = a.observe(snap())
        #expect(a.observe(snap(down: [row("new")])) == ["new"])
    }

    @Test func theMenuAndTheWindowListItAsADownloadNotRecentNorSharing() {
        let r = again()
        #expect(r.isDownload && !r.isSharing && r.finishedAt == nil)
        let s = snap(down: [r])
        #expect(s.recent.isEmpty)
        let l = StatusMenuController.layout(s, arrivals: DownloadArrivals())
        #expect(l.downloading.map(\.id) == ["t"] && l.queued.isEmpty)
        #expect(r.menuSubtitle == "42% · 3.1 MB/s · 4 min left")
        #expect(r.percent == 42)
    }
}
