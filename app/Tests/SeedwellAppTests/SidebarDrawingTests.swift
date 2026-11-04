import AppKit
import QuartzCore
import SwiftUI
import Testing
@testable import SeedwellApp

/// The sidebar's small marks, and QA's drawing of the sidebar's vibrant layers. Each test says the failure it catches.
///
/// `SIDEBAR_DRAWING_OUT=<folder>` keeps the pictures (as drawn for QA, with and without the marks, and drawn
/// in-process without the pass), the windows' layer trees and the contrast report; CI keeps them of a failed run.
@MainActor @Suite(.serialized) struct SidebarDrawingTests {
    final class Selection: ObservableObject {
        @Published var rows: Set<String> = []
    }

    /// Four sidebar rows made of the real views; without `marks` the same rows without their marks (the difference
    /// of the two pictures is where the marks are).
    struct Sidebar: View {
        @ObservedObject var selection: Selection
        let marks: Bool
        @State private var text = ""

        var body: some View {
            NavigationSplitView {
                List(selection: $selection.rows) {
                    Section("Chats") {
                        ChatSidebarRow(icon: "bubble.left", name: "Team", unread: 0, tag: marks ? SidebarTag(text: "agents", help: "") : nil)
                            .tag("tag")
                        HStack(spacing: 8) {
                            Image(systemName: "bubble.left").frame(width: 20).foregroundStyle(.secondary)
                            Text("Family")
                            Spacer(minLength: 4)
                            if marks { UnreadCapsule(count: 4) }
                        }
                        .tag("unread")
                    }
                    Section("Places") {
                        Label {
                            HStack(spacing: 6) {
                                Text("Downloads")
                                if marks { SidebarAccentMark { Circle().fill($0).frame(width: 6, height: 6) } }
                            }
                        } icon: { Image(systemName: "arrow.down.circle") }
                            .tag("flag")
                        HStack(spacing: 8) {
                            if marks {
                                SidebarAccentMark { Image(systemName: "star.fill").frame(width: 20).foregroundStyle($0.opacity(0.85)) }
                            } else {
                                Color.clear.frame(width: 20, height: 1)
                            }
                            Text("tracker.example")
                            Spacer(minLength: 4)
                        }
                        .tag("star")
                    }
                }
                .listStyle(.sidebar)
                .navigationSplitViewColumnWidth(min: 200, ideal: 232, max: 280)
            } detail: {
                TextField("Message", text: $text).padding(40)
            }
        }
    }

    final class Window: NSWindow {
        var key = true
        override var isKeyWindow: Bool { key }
        override var isMainWindow: Bool { key }
        override var canBecomeKey: Bool { false }
        override var canBecomeMain: Bool { false }
        override func constrainFrameRect(_ frameRect: NSRect, to screen: NSScreen?) -> NSRect { frameRect }
    }

    enum RowState: String, CaseIterable {
        case unselected, accent, grey, inactive
    }

    static let rows = ["tag", "unread", "flag", "star"]

    /// A sidebar in a window off the screens, in `state`, drawn as a QA shot draws it; its rows, and for each the
    /// layers drawn in it (what a failure on another Mac reports, as each macOS builds them its own way). With `keep`
    /// (a path without its extension) it also writes the window drawn in-process without the pass (`-raw.png`) and its
    /// whole layer tree (`-layers.txt`): what CI keeps of a failure, to see what another macOS drew.
    static func picture(dark: Bool, state: RowState, marks: Bool, keep: String? = nil) async throws -> (NSBitmapImageRep, [CGRect], [String]) {
        let selection = Selection()
        let w = Window(contentRect: NSRect(x: 0, y: 0, width: 640, height: 360),
                       styleMask: [.titled, .closable, .resizable, .fullSizeContentView], backing: .buffered, defer: false)
        w.isReleasedWhenClosed = false
        w.animationBehavior = .none
        w.appearance = NSAppearance(named: dark ? .darkAqua : .aqua)
        w.titlebarAppearsTransparent = true
        w.titleVisibility = .hidden
        w.contentViewController = NSHostingController(rootView: Sidebar(selection: selection, marks: marks))
        w.setContentSize(NSSize(width: 640, height: 360))
        w.setFrameOrigin(NSPoint(x: -30_000, y: -30_000))
        w.orderFront(nil)
        defer { w.contentViewController = nil; w.close() }
        let frame = try #require(w.contentView?.superview)
        for _ in 0..<6 { spinRunLoop(0.02); frame.layoutSubtreeIfNeeded() }
        let views = all(frame)
        let table = try #require(views.first { $0 is NSTableView } as? NSTableView)
        let field = views.first { ($0 as? NSTextField)?.isEditable == true }
        if state != .unselected { selection.rows = Set(rows) }
        switch state {
        case .unselected, .accent: w.makeFirstResponder(table)
        case .grey: w.makeFirstResponder(field)
        case .inactive:
            // Another window took the keys: what NSTableView does when its window resigns key (its rows drop the
            // accent), done here because this window is never really key.
            w.makeFirstResponder(table)
            w.key = false
            spinRunLoop(0.05)
            table.enumerateAvailableRowViews { row, _ in row.isEmphasized = false }
        }
        let rep = try #require(await SnapshotWindow.settled(limit: .seconds(2)) {
            frame.layoutSubtreeIfNeeded()
            return VibrancyPass.image(of: frame)
        })
        // The rows, in the picture's points (y up).
        let bands = (0..<table.numberOfRows).map { frame.convert(table.rect(ofRow: $0), from: table) }
        let root = try #require(frame.layer)
        if let keep {
            try SnapshotWindow.image(of: frame)?.representation(using: .png, properties: [:])?.write(to: URL(fileURLWithPath: "\(keep)-raw.png"))
            try tree(root).write(toFile: "\(keep)-layers.txt", atomically: true, encoding: .utf8)
        }
        return (rep, bands, bands.map { layers(in: $0, of: root) })
    }

    /// Every layer under `root`, one a line and indented by depth: its kind, place (the root's points), opacity,
    /// whether hidden, clipping, mask, background, contents and filters (compositing filter first).
    static func tree(_ root: CALayer) -> String {
        var out: [String] = []
        func walk(_ l: CALayer, _ depth: Int) {
            let filters = ([l.compositingFilter].compactMap { $0 }.map { ("compositing", $0) } + (l.filters ?? []).map { ("filter", $0) })
                .map { kind, f -> String in
                    guard let f = f as? NSObject, !(f is NSString), let m = Self.matrix(of: f) else { return "\(kind) \(f)" }
                    return "\(kind) \(f) \(m)"
                }
            var line = "\(String(repeating: "  ", count: depth))\(type(of: l)) at \(root.convert(l.bounds, from: l)) opacity \(l.opacity)"
            if l.isHidden { line += " hidden" }
            if l.masksToBounds { line += " clips (radius \(l.cornerRadius))" }
            if let mask = l.mask { line += " mask \(type(of: mask))" }
            if let bg = l.backgroundColor { line += " background \(bg.components ?? [])" }
            if let c = l.contents { line += " contents \(type(of: c))" }
            if !filters.isEmpty { line += " " + filters.joined(separator: ", ") }
            if let b = l.backgroundFilters, !b.isEmpty { line += " background filters \(b)" }
            // What the layer's own drawing (`render(in:)`, which VibrancyPass takes its coverage from) gives.
            if !filters.isEmpty || l.contents != nil { line += " own drawing covers \(coverage(of: l))" }
            out.append(line)
            for s in l.sublayers ?? [] { walk(s, depth + 1) }
        }
        walk(root, 0)
        return out.joined(separator: "\n")
    }

    /// The mean alpha of `layer` drawn alone at 2x, and its brightest pixel's (premultiplied) colour.
    static func coverage(of layer: CALayer) -> String {
        let b = layer.bounds
        let w = Int((b.width * 2).rounded(.up)), h = Int((b.height * 2).rounded(.up))
        guard w > 0, h > 0, w * h < 4_000_000, let space = CGColorSpace(name: CGColorSpace.sRGB),
              let ctx = CGContext(data: nil, width: w, height: h, bitsPerComponent: 8, bytesPerRow: w * 4, space: space,
                                  bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue),
              let data = ctx.data?.bindMemory(to: UInt8.self, capacity: w * h * 4) else { return "-" }
        ctx.scaleBy(x: 2, y: 2)
        ctx.translateBy(x: -b.minX, y: -b.minY)
        layer.render(in: ctx)
        var sum = 0, top = (0, 0, 0, 0)
        for i in 0..<(w * h) {
            let a = Int(data[i * 4 + 3])
            sum += a
            if a > top.3 { top = (Int(data[i * 4]), Int(data[i * 4 + 1]), Int(data[i * 4 + 2]), a) }
        }
        return "\(sum * 100 / (255 * w * h)) % (most \(top))"
    }

    /// The layers that draw something in `band` (the root's points), with their opacity, the opacity of the layers
    /// they are in, and their colour matrix.
    static func layers(in band: CGRect, of root: CALayer) -> String {
        var out: [String] = []
        func walk(_ l: CALayer, _ around: Float) {
            guard !l.isHidden else { return }
            let frame = root.convert(l.bounds, from: l)
            let matrices = ([l.compositingFilter].compactMap { $0 } + (l.filters ?? [])).compactMap { f -> String? in
                guard let f = f as? NSObject, !(f is NSString), let m = Self.matrix(of: f) else { return nil }
                return "\(f) \(m)"
            }
            if frame.intersects(band), l.contents != nil || l.backgroundColor != nil || !matrices.isEmpty {
                let bg = l.backgroundColor.map { " background \($0.components ?? [])" } ?? ""
                let drawn = matrices.isEmpty || frame.width * frame.height > 20_000 ? "" : " own drawing covers \(coverage(of: l))"
                out.append("\(type(of: l)) at \(frame) opacity \(l.opacity) in \(around)\(bg) \(matrices.joined(separator: ", "))\(drawn)")
            }
            for s in l.sublayers ?? [] { walk(s, around * l.opacity) }
        }
        walk(root, 1)
        return out.joined(separator: "\n")
    }

    /// A colour matrix filter's matrix (CAColorMatrix: rows for red, green, blue and alpha, 4 factors and a bias each).
    static func matrix(of filter: NSObject) -> [Float]? {
        guard "\(filter)".contains("ColorMatrix"), let value = filter.value(forKey: "inputColorMatrix") as? NSValue else { return nil }
        var m = [Float](repeating: 0, count: 20)
        m.withUnsafeMutableBytes { value.getValue($0.baseAddress!, size: 80) }
        return m
    }

    static func all(_ v: NSView) -> [NSView] { [v] + v.subviews.flatMap { all($0) } }

    struct Pixels {
        let rep: NSBitmapImageRep
        var width: Int { rep.pixelsWide }
        var height: Int { rep.pixelsHigh }
        /// RGB at a pixel (row 0 at the top), alpha laid over white.
        func rgb(_ x: Int, _ y: Int) -> SIMD3<Float> {
            let data = rep.bitmapData!
            let i = y * rep.bytesPerRow + x * 4
            let a = Float(data[i + 3]) / 255
            return SIMD3(Float(data[i]), Float(data[i + 1]), Float(data[i + 2])) + (1 - a) * 255
        }
    }

    static func distance(_ a: SIMD3<Float>, _ b: SIMD3<Float>) -> Float { ((a - b) * (a - b)).sum().squareRoot() }

    /// Where `with` differs from `without` inside `band` (points, y up), in pixels (row 0 at the top).
    static func differing(_ with: Pixels, _ without: Pixels, in band: CGRect) -> (x: Range<Int>, y: Range<Int>)? {
        let s = CGFloat(with.width) / with.rep.size.width
        let x0 = max(0, Int(band.minX * s)), x1 = min(with.width, Int(band.maxX * s))
        let y0 = max(0, with.height - Int(band.maxY * s)), y1 = min(with.height, with.height - Int(band.minY * s))
        var lx = Int.max, hx = Int.min, ly = Int.max, hy = Int.min
        for y in y0..<y1 {
            for x in x0..<x1 where distance(with.rgb(x, y), without.rgb(x, y)) > 12 {
                lx = min(lx, x); hx = max(hx, x); ly = min(ly, y); hy = max(hy, y)
            }
        }
        return lx <= hx ? (lx..<hx + 1, ly..<hy + 1) : nil
    }

    /// The most common colour of a box (its fill).
    static func fill(_ p: Pixels, _ box: (x: Range<Int>, y: Range<Int>)) -> SIMD3<Float> {
        var counts: [SIMD3<Float>: Int] = [:]
        for y in box.y { for x in box.x { counts[(p.rgb(x, y) / 4).rounded(.down) * 4, default: 0] += 1 } }
        return counts.max { $0.value < $1.value }!.key
    }

    /// The failure it catches: a mark the app draws itself (an unread count, a place's new-download dot, a favourite
    /// tracker's star) in the colour of its row's background, so it melts into the row in one state of the row: an
    /// accent mark in a selected row of a list that has the keyboard (an accent row) was accent on accent. Every state
    /// is drawn: not selected, an accent row, selected while the keyboard is elsewhere such as the composer (a grey
    /// row), and in a window that is not key; dark and light; as QA's shots draw (Reduce transparency, VibrancyPass).
    /// A channel's "agents" tag is drawn and reported, not required: its word is vibrant text, which only the window
    /// server draws, so an in-process picture of it is not what users see.
    @Test func theMarksReadInEveryStateOfTheirRow() async throws {
        _ = NSApplication.shared
        let restore = Self.reduceTransparency()
        defer { restore() }
        let out = ProcessInfo.processInfo.environment["SIDEBAR_DRAWING_OUT"]
        var report: [String] = []
        for dark in [true, false] {
            for state in RowState.allCases {
                let name = "\(state.rawValue)-\(dark ? "dark" : "light")"
                let (on, bands, layers) = try await Self.picture(dark: dark, state: state, marks: true, keep: out.map { "\($0)/sidebar-\(name)" })
                let (off, _, _) = try await Self.picture(dark: dark, state: state, marks: false, keep: out.map { "\($0)/sidebar-\(name)-no-marks" })
                if let out {
                    try on.representation(using: .png, properties: [:])?.write(to: URL(fileURLWithPath: "\(out)/sidebar-\(name).png"))
                    try off.representation(using: .png, properties: [:])?.write(to: URL(fileURLWithPath: "\(out)/sidebar-\(name)-no-marks.png"))
                }
                let a = Pixels(rep: on), b = Pixels(rep: off)
                // The table's rows: Chats, tag, unread, Places, flag, star.
                try #require(bands.count == 6, "\(name): \(bands.count) rows")
                for (row, i) in zip(Self.rows, [1, 2, 4, 5]) {
                    let band = bands[i]
                    // What a failure says besides its numbers: the row's layers as this macOS built them.
                    let drawn = "the row's layers:\n\(layers[i])"
                    guard let box = Self.differing(a, b, in: band) else {
                        if row != "tag" { Issue.record("\(name): the \(row) mark cannot be seen at all\n\(drawn)") }
                        report.append("\(name) \(row): not seen")
                        continue
                    }
                    let fill = Self.fill(a, box)
                    // Under the mark, the row as it is without it.
                    let row0 = b.rgb(box.x.lowerBound + box.x.count / 2, box.y.lowerBound + box.y.count / 2)
                    var text: Float = 0, mark: Float = 0
                    // The text: inside a capsule's round ends and edges, against its fill.
                    let inset = box.y.count / 2
                    let inner = (x: box.x.lowerBound + inset..<max(box.x.lowerBound + inset + 1, box.x.upperBound - inset),
                                 y: box.y.lowerBound + 2..<max(box.y.lowerBound + 3, box.y.upperBound - 2))
                    for y in box.y {
                        for x in box.x {
                            if inner.x.contains(x), inner.y.contains(y) { text = max(text, Self.distance(a.rgb(x, y), fill)) }
                            mark = max(mark, Self.distance(a.rgb(x, y), row0))
                        }
                    }
                    report.append("\(name) \(row): text on fill \(Int(text)), mark on row \(Int(mark))")
                    switch row {
                    case "tag":
                        // Reported, not required: its word is vibrant secondary text, which only the window server
                        // draws, and each macOS builds it differently, so how an in-process picture shows it is not
                        // what users see (macOS 26's runner drew the unselected tag not at all, macOS 27 clearly).
                        break
                    case "unread":
                        #expect(text >= 60, "\(name): the unread number does not read on its capsule (\(Int(text)))\n\(drawn)")
                        #expect(Self.distance(fill, row0) >= 60,
                                "\(name): the unread capsule melts into its row (\(Int(Self.distance(fill, row0))))\n\(drawn)")
                    default:
                        #expect(mark >= 60, "\(name): the \(row) mark melts into its row (\(Int(mark)))\n\(drawn)")
                    }
                }
            }
        }
        if let out { try report.joined(separator: "\n").write(toFile: "\(out)/sidebar-contrast.txt", atomically: true, encoding: .utf8) }
    }

    typealias Filter = (filter: NSObject, content: Bool, matrix: [Float])

    /// The colour matrix filters of the sidebar's vibrant layers as this macOS makes them, in every row state, dark and
    /// light: as a layer's compositing filter (icons, the selection) or among its filters (`content`: text).
    static func sidebarFilters() throws -> [String: Filter] {
        var filters: [String: Filter] = [:]
        for dark in [true, false] {
            for state in [RowState.unselected, .accent, .grey] {
                let selection = Selection()
                let w = Window(contentRect: NSRect(x: 0, y: 0, width: 640, height: 360), styleMask: [.titled, .fullSizeContentView],
                               backing: .buffered, defer: false)
                w.isReleasedWhenClosed = false
                w.animationBehavior = .none
                w.appearance = NSAppearance(named: dark ? .darkAqua : .aqua)
                w.contentViewController = NSHostingController(rootView: Sidebar(selection: selection, marks: true))
                w.setFrameOrigin(NSPoint(x: -30_000, y: -30_000))
                w.orderFront(nil)
                defer { w.contentViewController = nil; w.close() }
                for _ in 0..<6 { spinRunLoop(0.02); w.contentView?.superview?.layoutSubtreeIfNeeded() }
                let views = all(try #require(w.contentView?.superview))
                if state != .unselected { selection.rows = Set(rows) }
                w.makeFirstResponder(state == .grey ? views.first { ($0 as? NSTextField)?.isEditable == true } : views.first { $0 is NSTableView })
                for _ in 0..<6 { spinRunLoop(0.02); w.contentView?.superview?.layoutSubtreeIfNeeded() }
                func walk(_ l: CALayer) {
                    let all: [(Any, Bool)] = [l.compositingFilter].compactMap { $0 }.map { ($0, false) } + (l.filters ?? []).map { ($0, true) }
                    for (f, content) in all {
                        guard let f = f as? NSObject, !(f is NSString), let m = matrix(of: f) else { continue }
                        filters["\(f) \(content) \(m)"] = (f, content, m)
                    }
                    for s in l.sublayers ?? [] { walk(s) }
                }
                if let root = w.contentView?.superview?.layer { walk(root) }
            }
        }
        return filters
    }

    /// The failures it catches: QA's shot leaving a vibrant layer out of the picture (an in-process drawing skips the
    /// colour matrix the window server applies, so a sidebar label or icon is missing or raw black), and the shot
    /// taking a vibrant layer's colour from the layer itself instead of from what is under it (so the "agents" word
    /// came out one fixed grey, the grey of its pill, and the pill looked empty). For every vibrant filter this
    /// macOS's sidebar has, a layer carrying it is drawn by VibrancyPass over a dark and a light backdrop, once black
    /// and once white: it must change what is under it, and its own colour must not matter.
    @Test func vibrancyPassDrawsEachVibrantLayerFromWhatIsUnderIt() async throws {
        _ = NSApplication.shared
        let restore = Self.reduceTransparency()
        defer { restore() }
        // The ones that draw something (Core Animation draws nothing for a matrix that gives no alpha).
        let filters = try Self.sidebarFilters().filter { $0.value.matrix[18] > 0 || $0.value.matrix[19] > 0 }
        #expect(filters.count >= 3, "the sidebar's vibrant filters were not found (\(filters.count))")
        let w = Window(contentRect: NSRect(x: 0, y: 0, width: 8, height: 8), styleMask: [.borderless], backing: .buffered, defer: false)
        w.isReleasedWhenClosed = false
        w.setFrameOrigin(NSPoint(x: -30_000, y: -30_000))
        w.orderFront(nil)
        defer { w.close() }
        let space = try #require(CGColorSpace(name: CGColorSpace.sRGB))
        for (name, f) in filters {
            var changed: Float = 0
            for backdrop: SIMD3<Float> in [[28, 28, 28], [236, 236, 236]] {
                var drawn: [SIMD3<Float>] = []
                for own: CGFloat in [0, 1] {
                    let view = Backdrop(frame: NSRect(x: 0, y: 0, width: 8, height: 8), color: backdrop)
                    w.contentView = view
                    let layer = CALayer()
                    layer.frame = view.bounds
                    layer.backgroundColor = CGColor(colorSpace: space, components: [own, own, own, 1])
                    if f.content { layer.filters = [f.filter] } else { layer.compositingFilter = f.filter }
                    CATransaction.begin()
                    CATransaction.setDisableActions(true)
                    view.layer?.addSublayer(layer)
                    CATransaction.commit()
                    let rep = try #require(VibrancyPass.image(of: view))
                    drawn.append(Pixels(rep: rep).rgb(rep.pixelsWide / 2, rep.pixelsHigh / 2))
                }
                #expect(Self.distance(drawn[0], drawn[1]) <= 1,
                        "\(name) over \(backdrop): black gives \(drawn[0]), white \(drawn[1]); the layer's own colour must not count")
                changed = max(changed, Self.distance(drawn[0], backdrop))
            }
            #expect(changed >= 6, "\(name): the layer left what is under it as it was (\(changed)); the shot left it out")
        }
    }

    /// A view that is one colour, with room for layers over it.
    final class Backdrop: NSView {
        let color: SIMD3<Float>
        init(frame: NSRect, color: SIMD3<Float>) {
            self.color = color
            super.init(frame: frame)
            wantsLayer = true
        }
        required init?(coder: NSCoder) { nil }
        override func draw(_ dirtyRect: NSRect) {
            NSColor(srgbRed: CGFloat(color.x / 255), green: CGFloat(color.y / 255), blue: CGFloat(color.z / 255), alpha: 1).setFill()
            bounds.fill()
        }
    }

    /// Materials drawn as with Reduce transparency, as in a QA run (QuietRun), until the returned call puts it back.
    static func reduceTransparency() -> () -> Void {
        let sel = #selector(getter: NSWorkspace.accessibilityDisplayShouldReduceTransparency)
        guard let method = class_getInstanceMethod(NSWorkspace.self, sel) else { return {} }
        let reduce: @convention(block) (NSWorkspace) -> Bool = { _ in true }
        let original = method_setImplementation(method, imp_implementationWithBlock(reduce))
        return { method_setImplementation(method, original) }
    }
}
