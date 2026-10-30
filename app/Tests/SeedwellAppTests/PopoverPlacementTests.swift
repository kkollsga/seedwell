import AppKit
import Foundation
import SwiftUI
import Testing
@testable import SeedwellApp

// Get Info stays inside its popover and the screen (the Remove… question's rules are the core's: library_window_tests.rs).

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
