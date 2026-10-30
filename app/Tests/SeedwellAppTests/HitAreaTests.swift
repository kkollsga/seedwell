import AppKit
import SwiftUI
import Testing
@testable import SeedwellApp

/// A click anywhere in a small control's frame reaches that control: an unticked box is a clear fill and a hairline,
/// which hit-testing ignores unless the control gives itself a shape, and a chevron is a glyph a few points wide whose
/// misses once fell through to the row's tick. No event is sent: SwiftUI's accessibility hit test says which control
/// holds a point, from the same content shapes (without a `contentShape` the corners of the frame hold nothing; the
/// last test shows the probe can tell).
@MainActor @Suite struct HitAreaTests {
    /// The role and label of the element at each point of `view`, laid out alone in `size` (view points, origin bottom left).
    static func hits(_ view: some View, size: NSSize, at points: [NSPoint]) -> [String] {
        _ = NSApplication.shared
        // SwiftUI only builds its accessibility tree once an assistive client asks for it.
        NSApp.perform(NSSelectorFromString("accessibilitySetValue:forAttribute:"), with: true, with: "AXEnhancedUserInterface")
        let host = NSHostingView(rootView: view.frame(width: size.width, height: size.height))
        let window = NSWindow(contentRect: NSRect(origin: .zero, size: size), styleMask: [.borderless], backing: .buffered, defer: false)
        window.isReleasedWhenClosed = false
        window.contentView = host
        host.layoutSubtreeIfNeeded()
        defer { window.close() }
        return points.map { p in
            guard let o = host.accessibilityHitTest(window.convertPoint(toScreen: host.convert(p, to: nil))) as? NSObject else { return "nothing" }
            func ask(_ name: String) -> String {
                let sel = NSSelectorFromString(name)
                guard o.responds(to: sel), let v = o.perform(sel)?.takeUnretainedValue() else { return "" }
                return "\(v)"
            }
            return "\(ask("accessibilityRole")) \(ask("accessibilityLabel"))".trimmingCharacters(in: .whitespaces)
        }
    }

    /// The four corners (half a point in) and the centre of a `w` x `h` control centred in a 60 x 60 view.
    static func spots(_ w: CGFloat, _ h: CGFloat) -> [NSPoint] {
        let x0 = 30 - w / 2 + 0.5, x1 = 30 + w / 2 - 0.5, y0 = 30 - h / 2 + 0.5, y1 = 30 + h / 2 - 0.5
        return [NSPoint(x: x0, y: y0), NSPoint(x: x1, y: y0), NSPoint(x: x0, y: y1), NSPoint(x: x1, y: y1), NSPoint(x: 30, y: 30)]
    }

    static let square = NSSize(width: 60, height: 60)

    @Test(arguments: [TriState.off, .on, .mixed])
    func theBoxTakesClicksOnItsWholeFrame(state: TriState) {
        // The chooser and the checklists draw it 22 x 22 (the popover scales it, the hit area with it).
        let got = Self.hits(TriCheck(state: state) {}, size: Self.square, at: Self.spots(22, 22))
        #expect(got.allSatisfy { $0.hasPrefix("AXButton") }, "\(got)")
        // Just outside the frame is not the box.
        #expect(Self.hits(TriCheck(state: state) {}, size: Self.square, at: [NSPoint(x: 18, y: 30)]) != ["AXButton"])
    }

    /// The fold chevrons at the sizes they are used: the chooser (28 x 28), the checklist (26 x 28), a popover block's header (24 x 28).
    @Test(arguments: [(28.0, 28.0), (26.0, 28.0), (24.0, 28.0)])
    func theFoldChevronTakesClicksOnItsWholeFrame(size: (CGFloat, CGFloat)) {
        let view = FoldChevron(open: false, size: 10, width: size.0, height: size.1) {}
        let got = Self.hits(view, size: Self.square, at: Self.spots(size.0, size.1))
        #expect(got.allSatisfy { $0 == "AXButton Expand" }, "\(got)")
    }

    @Test func theRowTriangleTakesClicksOnItsWholeFrame() {
        let got = Self.hits(DisclosureTriangle(expanded: false) {}, size: Self.square, at: Self.spots(FileLayout.triangle, 22))
        #expect(got.allSatisfy { $0 == "AXButton Show files" }, "\(got)")
    }

    /// The control case: the same glyph with no shape of its own misses the corners, so the tests above can fail.
    @Test func aGlyphWithoutAShapeMissesItsCorners() {
        let bare = Button {} label: {
            Image(systemName: "chevron.right").font(.system(size: 10, weight: .semibold)).frame(width: 28, height: 28)
        }
        .buttonStyle(.plain).accessibilityLabel("Expand")
        let got = Self.hits(bare, size: Self.square, at: Self.spots(28, 28))
        #expect(got.last == "AXButton Expand", "the glyph itself is hit: \(got)")
        #expect(got.dropLast().allSatisfy { $0 != "AXButton Expand" }, "\(got)")
    }
}
