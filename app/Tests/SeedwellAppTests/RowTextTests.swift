import Foundation
import Testing
@testable import SeedwellApp

// The grey line under a row: what the engine says is shown as it is, plus what the app adds around it.

private func row(state: TransferState = .seeding, seedSecs: UInt64 = 0, error: String? = nil, group: SeedGroup? = .active,
                 ratio: Double = 0.94, goal: Double? = 2.0,
                 peers: UInt32 = 0, swarm: UInt32? = nil, known: UInt32 = 0, down: UInt64 = 0, up: UInt64 = 0,
                 pending: PendingStart? = nil, waiting: String? = nil, waitingIsError: Bool = false, halted: Bool = false,
                 retryIn: UInt64? = nil) -> TransferRow {
    TransferRow(
        id: "a", name: "Sintel.2010.1080p", displayName: "Sintel (2010)", kind: .torrent, category: .movie,
        sizeBytes: 1_000_000_000, doneBytes: 1_000_000_000, downRate: down, upRate: up, etaSecs: nil, trackerId: "public",
        trackerName: "Public", ratio: ratio, ratioGoal: goal, seedSecs: seedSecs, state: state, finishedAt: 1, savePath: "/x",
        fileExt: "mkv", seedGroup: group, error: error, retryInSecs: retryIn, movingPercent: nil, awaitingChoice: false, pending: pending, peers: peers,
        swarm: swarm, knownPeers: known, hasVideo: false, videoExt: "", fileCount: 1, lastActivity: nil,
        waitingNote: waiting, waitingIsError: waitingIsError, trackerHalted: halted)
}

@Suite("Row text")
struct RowTextTests {
    @Test func sharingRowsShowPeersAndRatioOnly() {
        // No rate (it stands out at the right of the name), tracker, goal, seeding time or swarm.
        #expect(row(ratio: 0.27, goal: nil, peers: 1, known: 250, up: 1_200_000).statusLine(.active) == "1 / 250 peers · Ratio 0.27")
        #expect(row(ratio: 0.27, goal: 1.0, peers: 1, up: 1_200_000).statusLine(.active) == "1 / 1 peers · Ratio 0.27 of 1.0")
        #expect(row(seedSecs: 3 * 86_400, ratio: 1.04, goal: nil).statusLine(.dormant) == "0 / 0 peers · Ratio 1.04")
        #expect(row(peers: 4, up: 45_000).statusLine(.popular) == "4 / 4 peers · Ratio 0.94 of 2.0")
        // Moving wins.
        var r = row(); r.movingPercent = 42
        #expect(r.statusLine(.active) == "Moving… 42%")
    }

    @Test @MainActor func theMenuCheckIsTickedWhileARowRuns() {
        #expect(StatusMenuController.isRunning(row(state: .downloading, group: nil)))
        #expect(StatusMenuController.isRunning(row(state: .queued, group: nil)))
        #expect(!StatusMenuController.isRunning(row(state: .paused, group: nil)))
        let p = PendingStart(startsAtMs: 0, remainingMs: 5_000, paused: true)
        #expect(!StatusMenuController.isRunning(row(state: .queued, group: nil, pending: p)))
    }

    @Test @MainActor func theToolbarLineShowsBothRates() {
        let s = Snapshot(downRate: 2_100_000, upRate: 340_000, downloading: [], queued: [], recent: [], sharingBusiest: [],
                         sharing: SharingCounts(popular: 0, active: 1, dormant: 0, error: 0, dead: 0, stopped: 0), sharingTotal: 1, trackers: [],
                         altSpeed: false, downLimit: nil, upLimit: nil, errorCount: 0, movingPercent: nil, attention: false, listenPort: 0,
                         connections: 0, online: true, offlineSinceMs: nil)
        #expect(LibraryModel.rates(s) == "↓ 2.1 MB/s · ↑ 340 KB/s")
    }

    @Test @MainActor func theMenusSharingLineEndsWithTheCombinedUploadRate() {
        func snap(up: UInt64, online: Bool = true, errors: UInt32 = 0) -> Snapshot {
            Snapshot(downRate: 0, upRate: up, downloading: [], queued: [], recent: [], sharingBusiest: [],
                     sharing: SharingCounts(popular: 0, active: 5, dormant: 0, error: 0, dead: 0, stopped: 0), sharingTotal: 5, trackers: [],
                     altSpeed: false, downLimit: nil, upLimit: nil, errorCount: errors, movingPercent: nil, attention: false, listenPort: 0,
                     connections: 0, online: online, offlineSinceMs: nil)
        }
        let busy = StatusMenuController.sharingContent(snap(up: 340_000))
        #expect(busy.title == "Sharing 5 torrents" && busy.trailing == "↑ 340 KB/s" && busy.rateActive == true)
        let idle = StatusMenuController.sharingContent(snap(up: 0, errors: 1))
        #expect(idle.title == "Sharing 5 torrents · 1 error" && idle.trailing == "–" && idle.rateActive == false)
        let offline = StatusMenuController.sharingContent(snap(up: 9_000, online: false))
        #expect(offline.title == "Sharing 5 torrents · Offline" && offline.trailing == "–")
    }

    @Test func theLiveRateStandsOutAtTheRightOfTheName() {
        #expect(row(peers: 4, up: 45_000).headlineRate(.popular) == "↑ 45 KB/s")
        #expect(row(up: 1_200_000).headlineRate(.active) == "↑ 1.2 MB/s")
        #expect(row(up: 0).headlineRate(.active) == nil)
        #expect(row(up: 0).rateSlot(.active)?.text == "–")
        #expect(row(state: .downloading, group: nil, down: 0).rateSlot(.download)?.text == "–")
        #expect(row(up: 1_000).rateSlot(.recent) == nil)
        #expect(row(state: .downloading, group: nil, down: 2_100_000).headlineRate(.download) == "↓ 2.1 MB/s")
        #expect(row(state: .downloading, group: nil, down: 0).headlineRate(.download) == nil)
        #expect(row(state: .paused, group: nil, down: 2_100_000).headlineRate(.download) == nil)
        var moving = row(up: 1_000); moving.movingPercent = 3
        #expect(moving.headlineRate(.active) == nil)
        #expect(row(up: 1_000).headlineRate(.recent) == nil)
    }

    @Test func aDownloadThatCannotFitSaysSoAndHowToRetry() {
        let msg = "Not enough space: 4.2 GB still to download, 1.1 GB free"
        let line = row(state: .error, error: msg, group: nil).statusLine(.download)
        #expect(line == msg + " · Resume to try again")
        // A running download has no retry hint.
        #expect(!row(state: .downloading, group: nil).statusLine(.download).contains("Resume"))
    }

    @Test func sharingTotalsLine() {
        // Against the whole torrent: 9.8 MB held of 17 MB, 8 of 13 files wanted and done.
        var r = row(ratio: 0.0)
        r.sizeBytes = 9_800_000; r.doneBytes = 9_800_000; r.totalBytes = 17_000_000; r.heldBytes = 9_800_000
        r.fileCount = 13; r.filesDone = 8; r.uploadedBytes = 0
        #expect(r.sharingTotalsLine == "8 of 13 files · 9.8 MB of 17 MB downloaded · 0 bytes uploaded")
        r.heldBytes = 17_000_000; r.filesDone = 13; r.uploadedBytes = 3_000_000_000
        #expect(r.sharingTotalsLine == "13 of 13 files · 17 MB downloaded · 3.0 GB uploaded")
        // One file leaves the file count out.
        var single = row()
        single.sizeBytes = 592_000_000; single.doneBytes = 592_000_000; single.totalBytes = 592_000_000; single.heldBytes = 592_000_000
        single.uploadedBytes = 3_000_000_000
        #expect(single.sharingTotalsLine == "592 MB downloaded · 3.0 GB uploaded")
        single.heldBytes = 100_000_000; single.doneBytes = 100_000_000
        #expect(single.sharingTotalsLine == "100 MB of 592 MB downloaded · 3.0 GB uploaded")
    }

    @Test func aStoppedRowDropsTheTrackerAndKeepsTheGoal() {
        #expect(row(state: .paused, group: .stopped, ratio: 0.4, goal: 1.0).statusLine(.stopped) == "Ratio 0.40 of 1.0 · Stopped")
    }

    @Test func downloadsEndWithThePeersAndLeaveTheRateToTheName() {
        var r = row(state: .downloading, group: nil, peers: 12, known: 340, down: 2_100_000)
        r.doneBytes = 73_000_000; r.sizeBytes = 85_000_000; r.etaSecs = 120
        #expect(r.statusLine(.download) == "73 of 85 MB — 2 minutes remaining · 12 / 340 peers")
    }

    @Test func downloadsFallBackToKnownPeersThenToTheBareCount() {
        func dl(peers: UInt32, known: UInt32 = 0, down: UInt64 = 0) -> TransferRow {
            row(state: .downloading, group: nil, peers: peers, known: known, down: down)
        }
        #expect(dl(peers: 4, known: 90, down: 1_000).liveSuffix(.download) == "4 / 90 peers")
        #expect(dl(peers: 4).liveSuffix(.download) == "4 / 4 peers")
        #expect(dl(peers: 0).liveSuffix(.download) == "0 / 0 peers")
    }

    @Test func aRunningDownloadAlwaysShowsBothPeerNumbers() {
        #expect(row(state: .downloading, group: nil, swarm: 250).liveSuffix(.download) == "0 / 0 peers")
        #expect(row(state: .paused, group: nil).liveSuffix(.download) == "")
        // Sharing rows have no live suffix; their line is `sharingLine`.
        #expect(row(peers: 4, up: 5_000_000).liveSuffix(.active) == "")
    }

    // An engine note replaces "Calculating time remaining…" when nothing is connected.
    @Test func aDownloadWithoutPeersSaysWhy() {
        var r = row(state: .downloading, group: nil, waiting: "Waiting for peers")
        r.doneBytes = 0; r.sizeBytes = 1_900_000_000
        #expect(r.statusLine(.download) == "0.0 of 1.9 GB — Waiting for peers · 0 / 0 peers")
        #expect(!r.statusLine(.download).contains("Calculating"))
        #expect(r.downloadWaiting?.note == "Waiting for peers")
    }

    @Test func aHaltedTrackerIsNamedInTheRowAndOffersAskingAgain() {
        var r = row(state: .downloading, group: nil, waiting: "Tracker: Unregistered torrent", waitingIsError: true, halted: true)
        r.doneBytes = 0; r.sizeBytes = 1_900_000_000
        #expect(r.statusLine(.download) == "0.0 of 1.9 GB — Tracker: Unregistered torrent · 0 / 0 peers")
        #expect(r.canAskAgain)
        // A failing (not halted) tracker is calm and says when it retries, with the attempt.
        var f = row(state: .downloading, group: nil, waiting: "Bad gateway", retryIn: 7_200)
        f.doneBytes = 0; f.errorTransient = true; f.retryAttempt = 3
        #expect(f.statusLine(.download).contains("Bad gateway · retrying in 2 h"))
        #expect(f.canAskAgain)
        f.retryInSecs = 0
        #expect(f.retryNote == "retrying now…")
        #expect(!row(state: .downloading, group: nil).canAskAgain)
    }

    @Test func aFlowingDownloadKeepsItsTimeRemaining() {
        var r = row(state: .downloading, group: nil, peers: 3, down: 1_000_000)
        r.doneBytes = 500_000_000; r.sizeBytes = 1_000_000_000; r.etaSecs = 500
        #expect(r.statusLine(.download).contains("remaining"))
    }
}

@Suite("Progress of a download again")
struct SessionProgressTests {
    /// A finished 4-file selection (600 MB held) that got a 200 MB episode ticked: the bar and the
    /// text count the new batch, not the 600 MB already there.
    private func again(sessionDone: UInt64, eta: UInt64? = 240) -> TransferRow {
        var r = row(state: .downloading, group: nil, down: 1_000_000)
        r.sizeBytes = 800_000_000; r.doneBytes = 600_000_000 + sessionDone; r.etaSecs = eta
        r.sessionSize = 200_000_000; r.sessionDone = sessionDone
        return r
    }

    @Test func startsAtZeroForTheNewFile() {
        let r = again(sessionDone: 0)
        #expect(r.fraction == 0)
        #expect(r.statusLine(.download, showTracker: false).hasPrefix("0.0 of 200 MB"))
    }

    @Test func countsTheNewBatchOnly() {
        let r = again(sessionDone: 50_000_000)
        #expect(r.fraction == 0.25)
        #expect(r.percent == 25)
        #expect(r.menuSubtitle == "25% · 1.0 MB/s · 4 min left")
    }

    @Test func rowsWithoutASessionUseTheWantedBytes() {
        var r = row(state: .downloading, group: nil)
        r.sizeBytes = 1_000_000_000; r.doneBytes = 420_000_000
        #expect(r.fraction == 0.42)
    }

    @Test func aSharingRowStillCountsTheWholeTorrent() {
        var r = row()
        r.sizeBytes = 592_000_000; r.doneBytes = 592_000_000; r.totalBytes = 1_400_000_000; r.heldBytes = 700_000_000
        r.fileCount = 24; r.filesDone = 4
        #expect(r.sharingTotalsLine.contains("700 MB of 1.4 GB downloaded"))
    }
}

@Suite("Menu line of a download")
struct MenuSubtitleTests {
    private func dl(rate: UInt64, eta: UInt64?, waiting: String? = nil) -> TransferRow {
        var r = row(state: .downloading, group: nil, down: rate, waiting: waiting)
        r.doneBytes = 420_000_000; r.sizeBytes = 1_000_000_000; r.etaSecs = eta
        return r
    }

    @Test func speedComesWithTimeLeft() {
        #expect(dl(rate: 3_100_000, eta: 240).menuSubtitle == "42% · 3.1 MB/s · 4 min left")
        #expect(dl(rate: 3_100_000, eta: nil).menuSubtitle == "42% · 3.1 MB/s · calculating…")
    }

    @Test func noSpeedShowsTheWaitingTextNotAStaleTime() {
        #expect(dl(rate: 0, eta: 240, waiting: "Waiting for peers").menuSubtitle == "42% · Waiting for peers")
        #expect(dl(rate: 0, eta: 240, waiting: "Tracker: Unregistered torrent").menuSubtitle == "42% · Tracker: Unregistered torrent")
        #expect(dl(rate: 0, eta: 240).menuSubtitle.contains("calculating"))
    }

    @Test func pausedAndLongNames() {
        #expect(row(state: .paused, group: nil).menuSubtitle.hasPrefix("Paused · "))
        let n = Fmt.middleTruncated(String(repeating: "a", count: 30) + String(repeating: "b", count: 30), to: 20)
        #expect(n.count == 20 && n.contains("…") && n.hasPrefix("aaaa") && n.hasSuffix("bbbb"))
        #expect(Fmt.middleTruncated("short", to: 20) == "short")
    }
}

@Suite("Tracker live status")
struct TrackerLiveTests {
    private func line(state: String, failures: UInt32 = 0, retryAt: UInt64? = nil, next: UInt64? = nil, manualOk: UInt64? = nil) -> TrackerLine {
        TrackerLine(url: "https://tracker.example/announce", tier: 0, status: "", lastAnnounce: nil, nextAnnounce: next, seeders: nil, leechers: nil,
                    state: state, failures: failures, retryAt: retryAt, lastSuccess: nil, manualOkAt: manualOk)
    }

    @Test func retryingCountsDownThenSaysNow() {
        let now = Date(timeIntervalSince1970: 1_000_000)
        let t = line(state: "backoff", failures: 2, retryAt: 1_000_000 + 540)
        #expect(TrackerLive.text(t, now: now) == "Retrying tracker in 9 min (attempt 3)")
        #expect(TrackerLive.text(t, now: now.addingTimeInterval(600)) == "Retrying tracker now…")
    }

    @Test func aWorkingTrackerSaysWhenItIsAskedNext() {
        let now = Date(timeIntervalSince1970: 1_000_000)
        #expect(TrackerLive.text(line(state: "working", next: 1_001_800), now: now).hasPrefix("Tracker OK · next announce"))
        #expect(TrackerLive.text(line(state: "waiting"), now: now) == "Announcing to tracker.example…")
    }

    @Test func theTrackersMinimumIntervalOffersAnnounceAnyway() {
        let now = Date(timeIntervalSince1970: 1_000_000)
        #expect(TrackerLive.allowedAt(line(state: "working", manualOk: 1_000_300), now: now) == 1_000_300)
        #expect(TrackerLive.allowedAt(line(state: "working", manualOk: 999_000), now: now) == nil)
    }
}

@Suite("Tracker words clear")
struct TrackerWordsClearTests {
    @Test func theRetryLineIsGoneOnceTheEngineReportsNoProblem() {
        var r = TransferRow(
            id: "a", name: "n", displayName: "n", kind: .torrent, category: .movie, sizeBytes: 10, doneBytes: 10, downRate: 0, upRate: 0, etaSecs: nil,
            trackerId: "public", trackerName: "Public", ratio: 0, ratioGoal: nil, seedSecs: 0, state: .seeding, finishedAt: 1, savePath: "/x", fileExt: "",
            seedGroup: .error, error: "Bad gateway", retryInSecs: 540, movingPercent: nil, awaitingChoice: false, pending: nil, peers: 0, swarm: nil,
            knownPeers: 0, hasVideo: false, videoExt: "", fileCount: 1, lastActivity: nil, waitingNote: nil, waitingIsError: false, trackerHalted: false)
        r.errorTransient = true
        #expect(r.statusLine(.error) == "Public · Bad gateway · retrying in 9 min")
        // The announce worked: the engine sends no error, no retry, a normal group.
        r.error = nil; r.retryInSecs = nil; r.errorTransient = false; r.seedGroup = .active
        #expect(!r.statusLine(.active).contains("retrying"))
        #expect(!r.statusLine(.active).contains("Bad gateway"))
        #expect(r.retryNote == nil)
    }
}
