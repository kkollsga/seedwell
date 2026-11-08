import AppKit
import SwiftUI
import Testing
@testable import SeedwellApp

/// How often SwiftUI measured the items.
@MainActor private final class Measured {
    var count = 0
}

/// A fixed-size item that counts its measurements.
private struct CountedItem: Layout {
    let measured: Measured
    let width: CGFloat

    func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) -> CGSize {
        MainActor.assumeIsolated { measured.count += 1 }
        return CGSize(width: width, height: 20)
    }

    func placeSubviews(in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) {}
}

@MainActor @Suite(.timeLimit(.minutes(1))) struct WrappingLineTests {
    /// Lays out `n` items 100 points wide in a row too narrow for them and counts the items' measurements.
    private func measurements(_ n: Int) -> Int {
        let measured = Measured()
        let view = WrappingLine(spacing: 8) {
            ForEach(0..<n, id: \.self) { _ in CountedItem(measured: measured, width: 100) { Color.clear } }
        }
        .frame(width: 150)
        let host = NSHostingView(rootView: view)
        host.frame = NSRect(x: 0, y: 0, width: 150, height: 2000)
        host.layoutSubtreeIfNeeded()
        return measured.count
    }

    /// The failure: a row's controls wrapped by a `ViewThatFits` nested in each fallback measured every level's
    /// candidates again for each candidate above it, so the cost doubled with each control: Settings › Transfers (rows
    /// of four and five controls) held the main thread 350 ms each time it was shown. Each item is measured a few
    /// times however many there are.
    @Test func eachItemIsMeasuredAFewTimesHoweverManyThereAre() {
        let six = measurements(6), twelve = measurements(12)
        #expect(six > 0)
        #expect(twelve <= six * 3, "6 items: \(six) measurements, 12 items: \(twelve)")
    }

    /// The failure: a Settings row wider than the window (the main window at its 820-point minimum) is clipped instead
    /// of wrapping. One line while it fits; then the first controls one per line and the rest on the last; two never wrap.
    @Test func controlsWrapOnlyWhenTheyDoNotFit() {
        let w: [CGFloat] = [40, 200, 70, 10, 70]
        let one = w.reduce(0, +) + 8 * 4
        #expect(WrappingLine.alone(widths: w, spacing: 8, width: one) == 0)
        #expect(WrappingLine.alone(widths: w, spacing: 8, width: nil) == 0)
        #expect(WrappingLine.alone(widths: w, spacing: 8, width: one - 1) == 1)
        #expect(WrappingLine.alone(widths: w, spacing: 8, width: 200) == 2, "70 + 10 + 70 with spacing fits beside nothing wider than 200")
        #expect(WrappingLine.alone(widths: w, spacing: 8, width: 10) == 3, "too narrow for anything: all but the last two alone")
        #expect(WrappingLine.alone(widths: [300, 300], spacing: 8, width: 100) == 0, "two never wrap")
    }
}
