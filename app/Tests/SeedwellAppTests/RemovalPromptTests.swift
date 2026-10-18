import AppKit
import Foundation
import SwiftUI
import Testing
@testable import SeedwellApp

// The inline Remove… question (SPEC 8.3) outlives focus changes; and Get Info stays inside its popover and the screen.

private func row(_ id: String) -> TransferRow {
    TransferRow(
        id: id, name: id, displayName: id, kind: .torrent, category: .movie,
        sizeBytes: 1, doneBytes: 0, downRate: 0, upRate: 0, etaSecs: nil, trackerId: "t", trackerName: "t", ratio: 0,
        ratioGoal: nil, seedSecs: 0, state: .downloading, finishedAt: nil, savePath: "/x", fileExt: "mkv", seedGroup: nil,
        error: nil, retryInSecs: nil, movingPercent: nil, awaitingChoice: false, pending: nil, peers: 0, swarm: nil,
        knownPeers: 0, hasVideo: false, videoExt: "", fileCount: 1, lastActivity: nil,
        waitingNote: nil, waitingIsError: false, trackerHalted: false)
}

@MainActor @Suite("Remove question")
struct RemovalPromptTests {
    @Test func survivesTheAppLosingAndRegainingFocus() {
        let p = RemovalPrompt()
        p.ask([row("a")], deleteFiles: false)
        #expect(p.pending != nil)
        // The app goes to the background and comes back; the window loses and regains key status.
        for name in [NSApplication.didResignActiveNotification, NSWindow.didResignKeyNotification,
                     NSApplication.didBecomeActiveNotification, NSWindow.didBecomeKeyNotification] {
            NotificationCenter.default.post(name: name, object: nil)
        }
        // The list refreshes with the same rows (a poll while the app was away).
        p.visibleRowsChanged(["a", "b"])
        #expect(p.pending?.ids == ["a"])
        #expect(p.isAnswerable(visible: ["a"]))
    }

    @Test func goesWhenAskedToOrWhenItsRowsAreGone() {
        let p = RemovalPrompt()
        p.ask([row("a")], deleteFiles: false)
        p.visibleRowsChanged(["b"])
        #expect(p.pending == nil, "its row left the screen")

        p.ask([row("a"), row("b")], deleteFiles: true)
        p.visibleRowsChanged(["b"])
        #expect(p.pending != nil, "one of the selected rows is still there")
        p.userChangedSection()
        #expect(p.pending == nil)

        p.ask([row("a")], deleteFiles: false)
        p.cancel()
        #expect(p.pending == nil)
    }

    @Test func returnOnlyAnswersWhileItsRowIsOnScreen() {
        let p = RemovalPrompt()
        #expect(!p.isAnswerable(visible: ["a"]), "nothing asked")
        p.ask([row("a")], deleteFiles: false)
        #expect(p.isAnswerable(visible: ["a"]))
        #expect(!p.isAnswerable(visible: ["z"]))
    }
}

@Suite("Get Info placement")
struct PopoverPlacementTests {
    // A 1440 x 900 screen (menu bar 24 pt: visible frame 0...876), a window filling it.
    private let screen = CGRect(x: 0, y: 0, width: 1440, height: 876)
    private let window = CGRect(x: 0, y: 0, width: 1440, height: 876)

    private func edge(rowTop: CGFloat, window w: CGRect? = nil, height: CGFloat = 500) -> Edge {
        PopoverPlacement.edge(row: CGRect(x: 300, y: rowTop, width: 800, height: 60), window: w ?? window, visible: screen, height: height)
    }

    @Test func opensBelowWhenThereIsRoomAndAboveWhenThereIsNot() {
        #expect(edge(rowTop: 80) == .bottom)
        #expect(edge(rowTop: 700) == .top)
    }

    @Test func prefersTheSideWithMoreRoomWhenNeitherFits() {
        #expect(edge(rowTop: 500, height: 760) == .top)    // 316 below, 500 above
        #expect(edge(rowTop: 300, height: 760) == .bottom) // 516 below, 300 above
    }

    @Test func aWindowNearTheBottomEdgeOfTheScreenOpensAbove() {
        let low = CGRect(x: 100, y: 0, width: 1000, height: 500)   // window bottom on the screen's edge
        #expect(edge(rowTop: 380, window: low) == .top)
    }

    @Test func theCardIsOneFixedWidthWhateverTheContent() {
        #expect(InfoCard.width == 460)
        #expect(InfoCard.contentWidth == 420)
        #expect(PopoverPlacement.width == InfoCard.width)
    }
}
