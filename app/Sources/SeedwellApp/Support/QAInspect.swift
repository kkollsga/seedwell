import AppKit

/// QA only: what a window shows, read without touching it (no event is posted; agents drive the app through the
/// QA commands, which call what a click or a key calls).
///
///     ui dump                          the accessibility tree of the window (role, label, frame in window points, y from the top)
///     ui state                         selection, file selection, expanded lists, the shared chooser models
///     ui overflow                      the views laid out past the window's left or right edge
///     ui ping                          how long the main run loop takes to serve a block queued from another thread
@MainActor enum QAInspect {
    struct Item {
        let role: String
        let label: String
        let frame: NSRect  // window points, y from the top
        let depth: Int
    }

    static func items(in win: NSWindow) -> [Item] {
        guard let root = win.contentView else { return [] }
        var out: [Item] = []
        let height = root.bounds.height
        func walk(_ o: Any, _ depth: Int) {
            guard depth < 40 else { return }
            if let a = o as? NSAccessibilityProtocol {
                let f = win.convertFromScreen(a.accessibilityFrame())
                let label = (a.accessibilityLabel() ?? "") + ((a.accessibilityValue() as? String).map { $0.isEmpty ? "" : " = " + $0 } ?? "")
                out.append(Item(role: a.accessibilityRole()?.rawValue ?? "?", label: label,
                                frame: NSRect(x: f.minX, y: height - f.maxY, width: f.width, height: f.height), depth: depth))
                for c in a.accessibilityChildren() ?? [] { walk(c, depth + 1) }
            }
        }
        // SwiftUI only builds its accessibility tree once an assistive client is asking for it.
        NSApp.perform(NSSelectorFromString("accessibilitySetValue:forAttribute:"), with: true, with: "AXEnhancedUserInterface")
        func views(_ v: NSView, _ depth: Int) {
            walk(v, depth)
            for s in v.subviews { views(s, depth + 1) }
        }
        views(root, 0)
        return out
    }

    static func dump(window win: NSWindow?, report: (String) -> Void) {
        guard let win else { return }
        report("window \(win.windowNumber) frame \(Int(win.frame.width))x\(Int(win.frame.height)) content \(win.contentView.map { "\(Int($0.frame.minX)),\(Int($0.frame.minY)) \(Int($0.bounds.width))x\(Int($0.bounds.height))" } ?? "-")")
        for i in items(in: win) where i.frame.width > 0 {
            report(String(repeating: " ", count: i.depth) + "\(i.role) '\(i.label)' x\(Int(i.frame.minX)) y\(Int(i.frame.minY)) \(Int(i.frame.width))x\(Int(i.frame.height))")
        }
    }

    /// `ui views`: the window's view tree from its frame view: class, frame, and the layer's class and effects (a
    /// layer with filters or a backdrop is composited by the window server and missing from an in-process drawing).
    static func views(window win: NSWindow?, report: (String) -> Void) {
        guard let root = win?.contentView?.superview else { return }
        func walk(_ v: NSView, _ depth: Int) {
            guard depth < 30 else { return }
            var line = String(repeating: " ", count: depth) + "\(type(of: v)) \(Int(v.frame.minX)),\(Int(v.frame.minY)) \(Int(v.frame.width))x\(Int(v.frame.height))"
            if v.isHidden { line += " hidden" }
            if let l = v.layer {
                line += " layer \(type(of: l))"
                if l.compositingFilter != nil { line += " compositing" }
                if !(l.filters ?? []).isEmpty { line += " filters" }
                if !(l.backgroundFilters ?? []).isEmpty { line += " backgroundFilters" }
                if let sub = l.sublayers, sub.contains(where: { String(describing: type(of: $0)).contains("Backdrop") }) { line += " backdrop" }
            }
            if let e = v as? NSVisualEffectView { line += " material \(e.material.rawValue) blending \(e.blendingMode.rawValue) state \(e.state.rawValue)" }
            if v.superview == nil || v.superview?.effectiveAppearance.name != v.effectiveAppearance.name { line += " appearance \(v.effectiveAppearance.name.rawValue)" }
            if v.allowsVibrancy { line += " vibrant" }
            if let l = v.layer {
                var effects: [String] = []
                func sub(_ x: CALayer, _ d: Int) {
                    guard d < 12 else { return }
                    for c in x.sublayers ?? [] where c.delegate == nil || !(c.delegate is NSView) {
                        if let f = c.compositingFilter {
                            var m = ""
                            if "\(f)".contains("ColorMatrix"), let v = (f as? NSObject)?.value(forKey: "inputColorMatrix") as? NSValue {
                                var x = [Float](repeating: 0, count: 20)
                                x.withUnsafeMutableBytes { v.getValue($0.baseAddress!, size: 80) }
                                m = " " + x.map { String(format: "%.2f", $0) }.joined(separator: " ")
                            }
                            effects.append("\(type(of: c)):\(f)\(m) hidden \(c.isHidden) \(Int(c.bounds.width))x\(Int(c.bounds.height))")
                        }
                        sub(c, d + 1)
                    }
                }
                sub(l, 0)
                if !effects.isEmpty { line += " sublayer-compositing [\(Set(effects).sorted().joined(separator: ", "))]" }
            }
            report(line)
            for s in v.subviews { walk(s, depth + 1) }
        }
        walk(root, 0)
    }

    /// `ui overflow`: the outermost views of the window laid out past its left or right edge, "overflow: none" when
    /// there are none. A page wider than the window (Settings › Rules at the 820-point minimum) made the whole content,
    /// sidebar and tab bar too, wider than the window and pushed it aside. Hidden views do not count, and a view
    /// inside a scroll view that scrolls sideways is the scroll view's to show.
    static func overflow(window win: NSWindow?) -> String {
        guard let win, let root = win.contentView else { return "overflow: no window" }
        let width = root.bounds.width
        var found: [String] = []
        func walk(_ v: NSView, _ depth: Int) {
            guard depth < 80, !v.isHidden, v.alphaValue > 0 else { return }
            let f = v.convert(v.bounds, to: root)
            if f.width > 0, f.height > 0, f.minX < -1 || f.maxX > width + 1 {
                found.append("\(type(of: v)) x \(Int(f.minX)) width \(Int(f.width))")
                return
            }
            if let scroll = v as? NSScrollView, scroll.hasHorizontalScroller { return }
            for s in v.subviews { walk(s, depth + 1) }
        }
        walk(root, 0)
        return found.isEmpty ? "overflow: none (window \(Int(width)))" : "overflow: \(found.prefix(4).joined(separator: "; ")) (window \(Int(width)))"
    }

    /// The row views of the window's lists (a sidebar's outline too) as drawn now: shown, not faded out, with their
    /// frames in their list's points.
    static func drawnRows(in win: NSWindow) -> [(list: NSTableView, frame: NSRect, what: String)] {
        var out: [(NSTableView, NSRect, String)] = []
        func walk(_ v: NSView, _ depth: Int) {
            guard depth < 60, !v.isHidden else { return }
            if let list = v as? NSTableView {
                for case let row as NSTableRowView in list.subviews where !row.isHidden {
                    let opacity = row.layer?.presentation()?.opacity ?? Float(row.alphaValue)
                    guard opacity > 0.01 else { continue }
                    let index = list.row(for: row)
                    let label = row.subviews.compactMap { ($0 as? NSTableCellView).flatMap { QAInspect.text(in: $0) } }.first ?? ""
                    let what = "row \(index) '\(label)' alpha \(String(format: "%.2f", row.alphaValue)) anim \(row.layer?.animationKeys()?.joined(separator: "+") ?? "none")"
                    out.append((list, row.layer?.presentation()?.frame ?? row.frame, what))
                }
                return
            }
            for s in v.subviews { walk(s, depth + 1) }
        }
        if let root = win.contentView { walk(root, 0) }
        return out
    }

    /// Two rows of one list drawn in one place ("2 rows at y 250"), none: empty. A row that lingers over another, or a
    /// disclosure caught while it closes, drew two channels' names over each other in a QA shot (2026-11-07).
    static func overlappingRows(in win: NSWindow) -> [String] {
        let rows = drawnRows(in: win)
        var found: [String] = []
        for (i, a) in rows.enumerated() {
            for b in rows[(i + 1)...] where a.list === b.list {
                let both = a.frame.intersection(b.frame)
                if !both.isNull, both.height > min(a.frame.height, b.frame.height) / 2 { found.append("2 rows at y \(Int(a.frame.minY)) (\(a.what); \(b.what))") }
            }
        }
        return found
    }

    /// The first words a view's accessibility tree says (a row's name).
    static func text(in v: NSView) -> String? {
        func walk(_ o: Any, _ depth: Int) -> String? {
            guard depth < 12, let a = o as? NSAccessibilityProtocol else { return nil }
            if let l = a.accessibilityLabel(), !l.isEmpty { return l }
            for c in a.accessibilityChildren() ?? [] { if let t = walk(c, depth + 1) { return t } }
            return nil
        }
        return walk(v, 0)
    }

    /// A list's rows are moving (a disclosure opening or closing, a row fading): their layers have animations.
    static func rowsAnimating(in win: NSWindow) -> Bool {
        func walk(_ v: NSView, _ depth: Int) -> Bool {
            guard depth < 60, !v.isHidden else { return false }
            if let list = v as? NSTableView {
                return list.subviews.contains { !($0.layer?.animationKeys() ?? []).isEmpty }
            }
            return v.subviews.contains { walk($0, depth + 1) }
        }
        return win.contentView.map { walk($0, 0) } ?? false
    }

    static func ping(report: @escaping @MainActor (String) -> Void) {
        let t0 = Date()
        DispatchQueue.global().async {
            let queued = Date()
            Task { @MainActor in report("ping: main run loop served in \(Int(Date().timeIntervalSince(queued) * 1000)) ms (since command \(Int(Date().timeIntervalSince(t0) * 1000)) ms)") }
        }
    }
}
