import AppKit
import SwiftUI

/// Where Get Info opens: below the row when the screen has room for it there, else above it, else on
/// the side with more room. The popover is a fixed width, so only the height decides.
enum PopoverPlacement {
    /// The popover's width, matching `InfoCard`.
    static let width: CGFloat = 460
    /// Room the popover needs beyond its content: the arrow and a margin to the screen edge.
    static let margin: CGFloat = 24

    /// `row`: the row's frame in window content coordinates (origin top left, y down). `window`: the window's
    /// frame on screen (origin bottom left, y up). `visible`: the screen's visible frame, same space.
    /// `height`: the popover's height.
    static func edge(row: CGRect, window: CGRect, visible: CGRect, height: CGFloat) -> Edge {
        let top = window.maxY - row.minY       // screen y of the row's top edge
        let bottom = window.maxY - row.maxY    // and of its bottom edge
        let below = bottom - visible.minY
        let above = visible.maxY - top
        let need = height + margin
        if below >= need { return .bottom }
        if above >= need { return .top }
        return below >= above ? .bottom : .top
    }

    /// The same for a view on screen now; `.bottom` when there is no window to measure.
    @MainActor static func edge(rowInWindow row: CGRect, height: CGFloat) -> Edge {
        guard let w = NSApp.keyWindow ?? NSApp.mainWindow ?? NSApp.windows.first(where: \.isVisible),
              let screen = w.screen ?? NSScreen.main else { return .bottom }
        return edge(row: row, window: w.frame, visible: screen.visibleFrame, height: height)
    }
}
