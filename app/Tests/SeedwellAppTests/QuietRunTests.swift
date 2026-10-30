import AppKit
import Testing
@testable import SeedwellApp

/// A QA-control run keeps every window off the screens (QuietRun): the rule that moves a frame, checked without a
/// window (the run's hooks are never installed in the tests; the normal app never installs them).
@Suite struct QuietRunTests {
    // A laptop screen and an external display to its left and above, as macOS arranges them.
    private let screens = [CGRect(x: 0, y: 0, width: 1512, height: 982), CGRect(x: -2560, y: 400, width: 2560, height: 1440)]

    @Test func aFrameOnAnyScreenGoesOffEveryScreen() {
        for frame in [CGRect(x: 166, y: 81, width: 1180, height: 820),       // centred on the laptop
                      CGRect(x: -2000, y: 900, width: 640, height: 640),     // on the external display
                      CGRect(x: 1400, y: 900, width: 300, height: 200)] {    // only a corner on a screen
            let moved = QuietRun.offscreen(frame, screens: screens)
            #expect(moved.size == frame.size)
            #expect(!screens.contains { $0.intersects(moved) }, "\(frame) -> \(moved)")
        }
    }

    @Test func aFrameOffTheScreensStaysWhereItIs() {
        let off = CGRect(x: -39_834, y: -39_919, width: 1180, height: 820)
        #expect(QuietRun.offscreen(off, screens: screens) == off)
        // Moved twice is moved once: frames set again and again (a resize, a sheet) do not drift.
        let once = QuietRun.offscreen(CGRect(x: 10, y: 10, width: 100, height: 100), screens: screens)
        #expect(QuietRun.offscreen(once, screens: screens) == once)
    }

    @Test func windowsKeepTheirPlacesRelativeToEachOther() {
        // A sheet over its window and a popover at a row move by the same offset.
        let window = CGRect(x: 166, y: 81, width: 1180, height: 820)
        let sheet = CGRect(x: 456, y: 600, width: 600, height: 250)
        let a = QuietRun.offscreen(window, screens: screens), b = QuietRun.offscreen(sheet, screens: screens)
        #expect(b.minX - a.minX == sheet.minX - window.minX)
        #expect(b.minY - a.minY == sheet.minY - window.minY)
    }

    @Test func anEmptyFrameIsLeftAlone() {
        let empty = CGRect(x: 10, y: 10, width: 0, height: 0)
        #expect(QuietRun.offscreen(empty, screens: screens) == empty)
    }

    @Test func aNormalRunMeasuresTheFrameAsItIs() {
        // Get Info measures the room around the row on screen; only a quiet run moves the frame back first.
        #expect(!QuietRun.on)
        let off = CGRect(x: -39_834, y: -39_919, width: 1180, height: 820)
        #expect(QuietRun.onscreen(off, screens: screens) == off)
    }
}
