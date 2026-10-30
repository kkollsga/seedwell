import Foundation
import Testing
@testable import SeedwellApp

// The countdown of a torrent that waits to start (SPEC 7A.7, 8.6): the numbers the notifier, the rows and
// the menu show, and what the notifier model keeps between two looks at the core.

private let t0 = Date(timeIntervalSince1970: 1_800_000_000)

private func wait(startsIn secs: Double, paused: Bool = false, left: Double? = nil) -> PendingStart {
    PendingStart(startsAtMs: UInt64((t0.timeIntervalSince1970 + secs) * 1000), remainingMs: UInt64((left ?? secs) * 1000), paused: paused)
}

private func row(_ id: String = "a", title: String = "Night of the Living Dead (1968)", pending: PendingStart? = wait(startsIn: 10),
                 awaiting: Bool = false, category: SeedwellApp.Category = .movie, size: UInt64 = 1_000_000_000,
                 path: String = "/Users/me/Media/Movies/Night.of.the.Living.Dead.1968") -> TransferRow {
    TransferRow(
        id: id, name: "Night.of.the.Living.Dead.1968", displayName: title, kind: .torrent, category: category,
        sizeBytes: size, doneBytes: 0, downRate: 0, upRate: 0, etaSecs: nil, trackerId: "public",
        trackerName: "Public", ratio: 0, ratioGoal: nil, seedSecs: 0, state: .queued, finishedAt: nil, savePath: path,
        fileExt: "mkv", seedGroup: nil, error: nil, retryInSecs: nil, movingPercent: nil, awaitingChoice: awaiting,
        pending: pending, peers: 0, swarm: nil, knownPeers: 0, hasVideo: false, videoExt: "", fileCount: 1, lastActivity: nil, waitingNote: nil, waitingIsError: false, trackerHalted: false)
}

@Suite("Starting countdown")
struct StartingCountdownTests {
    @Test func aRunningCountdownFollowsTheClockAndNeverGoesNegative() {
        let p = wait(startsIn: 10)
        #expect(p.remaining(at: t0) == 10)
        #expect(p.remaining(at: t0.addingTimeInterval(3.5)) == 6.5)
        #expect(p.remaining(at: t0.addingTimeInterval(12)) == 0)
    }

    @Test func aStoppedCountdownStandsStill() {
        let p = wait(startsIn: 99, paused: true, left: 6.25)
        #expect(p.remaining(at: t0) == 6.25)
        #expect(p.remaining(at: t0.addingTimeInterval(500)) == 6.25)
    }

    @Test func theLineCountsWholeSecondsDownAndOnlySaysZeroWhenItStarts() {
        #expect(StartingText.line(remaining: 10, paused: false) == "Starting in 10 s")
        #expect(StartingText.line(remaining: 7.01, paused: false) == "Starting in 8 s", "rounds up: 7.01 s left is still 8 on the clock")
        #expect(StartingText.line(remaining: 0.2, paused: false) == "Starting in 1 s")
        #expect(StartingText.line(remaining: 0, paused: false) == "Starting…")
        #expect(StartingText.line(remaining: 4, paused: true) == "Paused")
    }

    @Test func theDetailNamesWhatItIsAndWhereItGoes() {
        #expect(StartingText.detail(category: "Movie", folder: "~/Media/Movies") == "Movie · ~/Media/Movies")
        #expect(StartingText.detail(category: "Magnet link", folder: nil) == "Magnet link")
    }
}

@MainActor @Suite("Starting model")
struct StartingModelTests {
    @Test func itKeepsOneItemPerPendingTorrentWithItsOwnCountdown() {
        let m = StartingModel()
        m.sync([row("a", pending: wait(startsIn: 10)), row("b", title: "Sintel (2010)", pending: wait(startsIn: 7)), row("c", pending: nil)], now: t0, home: "/Users/me")
        #expect(m.items.map(\.id) == ["a", "b"])
        #expect(m.items[0].remaining(at: t0) == 10 && m.items[1].remaining(at: t0) == 7)
        #expect(m.items[0].line(at: t0.addingTimeInterval(3)) == "Starting in 7 s")
        #expect(m.items[1].line(at: t0.addingTimeInterval(3)) == "Starting in 4 s", "each torrent counts to its own deadline")
        #expect(m.items[0].detail == "Movie · 1.0 GB · ~/Media/Movies")
    }

    @Test func aTorrentThatNeedsAChoiceIsInThePopoverButNotOnTheCountdown() {
        let m = StartingModel()
        m.sync([row("a", pending: nil, awaiting: true)], now: t0)
        #expect(m.items.count == 1 && m.items[0].awaiting && m.wantsToShow)
        #expect(m.items[0].line(at: t0) == "Choose what to download")
    }

    @Test func theRingStartsFullAndEmptiesAtTheDeadline() {
        let m = StartingModel()
        m.sync([row(pending: wait(startsIn: 10))], now: t0)
        let item = m.items[0]
        #expect(item.fraction(at: t0) == 1)
        #expect(abs(item.fraction(at: t0.addingTimeInterval(5)) - 0.5) < 1e-9)
        #expect(item.fraction(at: t0.addingTimeInterval(10)) == 0)
        #expect(item.fraction(at: t0.addingTimeInterval(30)) == 0)
    }

    @Test func theRingKeepsItsScaleWhenTheModelLooksAgain() {
        let m = StartingModel()
        m.sync([row(pending: wait(startsIn: 10))], now: t0)
        m.sync([row(pending: wait(startsIn: 10))], now: t0.addingTimeInterval(4))
        #expect(m.items[0].total == 10, "the full circle is the wait as first seen, not what is left at the next look")
        #expect(abs(m.items[0].fraction(at: t0.addingTimeInterval(4)) - 0.6) < 1e-9)
    }

    @Test func pausingStopsTheNumberAndResumingRunsItAgain() {
        let m = StartingModel()
        m.sync([row(pending: wait(startsIn: 10))], now: t0)
        m.sync([row(pending: wait(startsIn: 99, paused: true, left: 6))], now: t0.addingTimeInterval(4))
        #expect(m.items[0].paused)
        #expect(m.items[0].line(at: t0.addingTimeInterval(60)) == "Paused")
        #expect(m.items[0].remaining(at: t0.addingTimeInterval(60)) == 6)
        // resumed: the engine says it starts 11.5 s after t0 (6.5 s after this look)
        m.sync([row(pending: wait(startsIn: 11.5))], now: t0.addingTimeInterval(5))
        #expect(!m.items[0].paused)
        #expect(abs(m.items[0].remaining(at: t0.addingTimeInterval(6)) - 5.5) < 1e-9)
    }

    @Test func aTorrentThatStartedLeavesTheList() {
        let m = StartingModel()
        m.sync([row("a"), row("b")], now: t0)
        m.sync([row("b")], now: t0.addingTimeInterval(1))
        #expect(m.items.map(\.id) == ["b"])
        m.sync([], now: t0.addingTimeInterval(2))
        #expect(m.items.isEmpty && !m.wantsToShow)
    }

    @Test func aClosedNotifierStaysClosedUntilANewTorrentWaits() {
        let m = StartingModel()
        m.sync([row("a")], now: t0)
        #expect(m.wantsToShow)
        m.dismissAll()
        m.sync([row("a")], now: t0.addingTimeInterval(1))
        #expect(!m.wantsToShow, "the same torrent does not bring it back")
        m.sync([row("a"), row("b", title: "Sintel (2010)")], now: t0.addingTimeInterval(2))
        #expect(m.wantsToShow, "a new one does")
        m.dismissAll()
        m.sync([], now: t0.addingTimeInterval(20))
        m.sync([row("a")], now: t0.addingTimeInterval(21))
        #expect(m.wantsToShow, "once it left the list, a torrent with the same id is new again")
    }

    @Test func aMagnetLinkIsNamedAsOneBecauseItsFilesAreNotKnown() {
        let m = StartingModel()
        m.sync([row(category: .other, size: 0, path: "/Users/me/Downloads/abcdef")], now: t0, home: "/Users/me")
        #expect(m.items[0].kind == "Magnet link" && m.items[0].detail == "Magnet link · ~/Downloads")
    }

    @Test func theFolderIsWhereTheFilesGoAbbreviatedWithTheHomeFolder() {
        #expect(StartingModel.folder(of: "/Users/me/Media/Movies/X", home: "/Users/me") == "~/Media/Movies")
        #expect(StartingModel.folder(of: "/Users/me/X", home: "/Users/me") == "~")
        #expect(StartingModel.folder(of: "/Volumes/Big/Movies/X", home: "/Users/me") == "/Volumes/Big/Movies")
        #expect(StartingModel.folder(of: "", home: "/Users/me") == nil)
    }

    @Test func aPendingRowSaysWhenItStartsAndStaysOutOfPlayAndFileLists() {
        // Its line ("Starting in 7 s") is the core's (library_rows.rs).
        let r = row(pending: wait(startsIn: 7))
        #expect(r.isHeld && !r.showsPlayButton && !r.mayHoldVideo)
    }
}
