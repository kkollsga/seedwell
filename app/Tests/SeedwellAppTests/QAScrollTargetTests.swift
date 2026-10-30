import AppKit
import Testing
@testable import SeedwellApp

// The QA harness's `scroll` and `bench-scroll` drive the same scroll view. An open file list taller than about 13 rows
// scrolls inside the window's list, so the lookup must find a scroll view nested in another one (`bench-scroll` once
// stopped at the outer list and reported "nothing to scroll" for the 2,000-file archive).

@MainActor
@Suite("QA scroll target")
struct QAScrollTargetTests {
    private func scrollView(width: CGFloat, height: CGFloat, content: CGFloat) -> NSScrollView {
        let sv = NSScrollView(frame: NSRect(x: 0, y: 0, width: width, height: height))
        sv.documentView = NSView(frame: NSRect(x: 0, y: 0, width: width, height: content))
        return sv
    }

    @Test func aFileListScrollingInsideTheWindowListIsFound() {
        let root = NSView(frame: NSRect(x: 0, y: 0, width: 900, height: 800))
        let outer = scrollView(width: 900, height: 800, content: 600)   // the window's list fits
        let inner = scrollView(width: 860, height: 280, content: 42_000) // a 2,000-file list
        outer.documentView!.addSubview(inner)
        root.addSubview(outer)
        #expect(QAControl.scrollTarget(in: root) === inner)
    }

    @Test func theWidestScrollableViewWinsAndNothingScrollableIsNil() {
        let root = NSView(frame: NSRect(x: 0, y: 0, width: 900, height: 800))
        let outer = scrollView(width: 900, height: 800, content: 5_000)
        let inner = scrollView(width: 860, height: 280, content: 42_000)
        outer.documentView!.addSubview(inner)
        root.addSubview(outer)
        #expect(QAControl.scrollTarget(in: root) === outer)
        let still = NSView(frame: root.frame)
        still.addSubview(scrollView(width: 900, height: 800, content: 600))
        #expect(QAControl.scrollTarget(in: still) == nil)
    }
}
