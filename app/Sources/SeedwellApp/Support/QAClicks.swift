import AppKit

/// QA only: real mouse clicks inside the app's own main window. The events are built with
/// `NSEvent.mouseEvent` and posted to this application's queue, so the window's hit-testing and
/// SwiftUI's gestures run as for a user, and the real pointer never moves (no global CGEvent).
///
///     ui dump                          the accessibility tree of the window (role, label, frame in window points, y from the top)
///     click <label> [command] [shift] [double] [hover] [x=<dx>] [n=<k>] [gap=<ms>] [role=AXButton]
///                                      click the k-th (default 1st) element whose label contains <label>, dx points
///                                      from its left edge (default: its centre)
///     click at=<x>,<y> [flags]         a click at window point (y from the top)
///     ui state                         selection, file selection, expanded lists, handlers alive
///     ui ping                          how long the main run loop takes to serve a block queued from another thread
@MainActor enum QAClicks {
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

    static func command(_ arg: String, window win: NSWindow?, report: @escaping (String) -> Void) {
        guard let win else { return report("click: no window") }
        countEvents()
        let words = arg.split(separator: " ").map(String.init)
        var flags: NSEvent.ModifierFlags = []
        var double = false, hover = false, leadMs = 120, dx: Double?, n = 1, gap = 30, role: String?, at: NSPoint?
        var label: [String] = []
        for w in words {
            if w == "command" { flags.insert(.command) }
            else if w == "shift" { flags.insert(.shift) }
            else if w == "double" { double = true }
            else if w == "hover" { hover = true }
            else if w.hasPrefix("lead="), let v = Int(w.dropFirst(5)) { hover = true; leadMs = v }
            else if w.hasPrefix("x="), let v = Double(w.dropFirst(2)) { dx = v }
            else if w.hasPrefix("n="), let v = Int(w.dropFirst(2)) { n = v }
            else if w.hasPrefix("gap="), let v = Int(w.dropFirst(4)) { gap = v }
            else if w.hasPrefix("role=") { role = String(w.dropFirst(5)) }
            else if w.hasPrefix("at=") {
                let p = w.dropFirst(3).split(separator: ",").compactMap { Double($0) }
                if p.count == 2 { at = NSPoint(x: p[0], y: p[1]) }
            } else { label.append(w) }
        }
        let height = win.contentView?.bounds.height ?? 0
        var point: NSPoint
        if let at { point = at } else {
            let text = label.joined(separator: " ")
            let hits = items(in: win).filter { $0.label.localizedCaseInsensitiveContains(text) && (role == nil || $0.role == role) && $0.frame.width > 0 }
            guard hits.indices.contains(n - 1) else { return report("click: no element '\(text)' #\(n) (\(hits.count) found)") }
            let f = hits[n - 1].frame
            point = NSPoint(x: dx.map { f.minX + $0 } ?? f.midX, y: f.midY)
        }
        report("click down@\(String(format: "%.3f", ProcessInfo.processInfo.systemUptime + Double(hover ? leadMs : 0) / 1000)) at (\(Int(point.x)), \(Int(point.y)))\(double ? " x2" : "")\(flags.isEmpty ? "" : " \(words.filter { $0 == "command" || $0 == "shift" })")")
        let loc = NSPoint(x: point.x, y: height - point.y)
        let clicks = double ? 2 : 1
        // `hover`: the pointer arrives first (mouse-moved events), as it does for a person, so row hover states run.
        let lead = hover ? leadMs : 0
        if hover { for k in 0..<3 { post(.mouseMoved, NSPoint(x: loc.x - Double(6 - k * 2), y: loc.y), flags, win, count: 0, after: k * min(30, leadMs / 4)) } }
        for k in 1...clicks {
            let base = lead + (k - 1) * (gap * 2)
            post(.leftMouseDown, loc, flags, win, count: k, after: base)
            post(.leftMouseUp, loc, flags, win, count: k, after: base + gap)
        }
    }

    private static func post(_ type: NSEvent.EventType, _ loc: NSPoint, _ flags: NSEvent.ModifierFlags, _ win: NSWindow, count: Int, after ms: Int) {
        let send = {
            if let e = NSEvent.mouseEvent(with: type, location: loc, modifierFlags: flags, timestamp: ProcessInfo.processInfo.systemUptime,
                                          windowNumber: win.windowNumber, context: nil, eventNumber: 0, clickCount: count,
                                          pressure: type == .leftMouseDown ? 1 : 0) {
                NSApp.postEvent(e, atStart: false)
            }
        }
        if ms == 0 { send() } else { DispatchQueue.main.asyncAfter(deadline: .now() + .milliseconds(ms)) { send() } }
    }

    static var downs = 0, ups = 0, moves = 0
    private static var monitor: Any?
    /// Counts the events that reached the app's queue (so a lost click can be told from an undelivered one).
    static func countEvents() {
        guard monitor == nil else { return }
        monitor = NSEvent.addLocalMonitorForEvents(matching: [.leftMouseDown, .leftMouseUp, .mouseMoved]) { e in
            MainActor.assumeIsolated {
                switch e.type { case .leftMouseDown: downs += 1; case .leftMouseUp: ups += 1; default: moves += 1 }
            }
            return e
        }
    }

    static func dump(window win: NSWindow?, report: (String) -> Void) {
        guard let win else { return }
        report("window \(win.windowNumber) frame \(Int(win.frame.width))x\(Int(win.frame.height)) content \(win.contentView.map { "\(Int($0.frame.minX)),\(Int($0.frame.minY)) \(Int($0.bounds.width))x\(Int($0.bounds.height))" } ?? "-")")
        for i in items(in: win) where i.frame.width > 0 {
            report(String(repeating: " ", count: i.depth) + "\(i.role) '\(i.label)' x\(Int(i.frame.minX)) y\(Int(i.frame.minY)) \(Int(i.frame.width))x\(Int(i.frame.height))")
        }
    }

    static func ping(report: @escaping @MainActor (String) -> Void) {
        let t0 = Date()
        DispatchQueue.global().async {
            let queued = Date()
            Task { @MainActor in report("ping: main run loop served in \(Int(Date().timeIntervalSince(queued) * 1000)) ms (since command \(Int(Date().timeIntervalSince(t0) * 1000)) ms)") }
        }
    }
}
