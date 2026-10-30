import AppKit

/// QA only: what a window shows, read without touching it (no event is posted; agents drive the app through the
/// QA commands, which call what a click or a key calls).
///
///     ui dump                          the accessibility tree of the window (role, label, frame in window points, y from the top)
///     ui state                         selection, file selection, expanded lists, the shared chooser models
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

    static func ping(report: @escaping @MainActor (String) -> Void) {
        let t0 = Date()
        DispatchQueue.global().async {
            let queued = Date()
            Task { @MainActor in report("ping: main run loop served in \(Int(Date().timeIntervalSince(queued) * 1000)) ms (since command \(Int(Date().timeIntervalSince(t0) * 1000)) ms)") }
        }
    }
}
