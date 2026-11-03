import AppKit
import Metal
import QuartzCore
import SwiftUI
import Testing
@testable import SeedwellApp

/// The sidebar's small marks (a channel's "agents" tag, an unread count, a place's new-download dot, a favourite
/// tracker's star) stay readable in every state of their row: not selected, selected in a list that has the keyboard
/// (an accent row), selected while the keyboard is elsewhere such as the composer (a grey row), and in a window that
/// is not key; dark and light. Drawn as QA's shots draw (Reduce transparency, VibrancyPass), and VibrancyPass itself
/// checked against Core Animation's own renderer, which composites as the window server does.
///
/// `SIDEBAR_DRAWING_OUT=<folder>` keeps the pictures.
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

    /// A sidebar in a window off the screens, in `state`, drawn as a QA shot draws it.
    static func picture(dark: Bool, state: RowState, marks: Bool) async throws -> (NSBitmapImageRep, [CGRect]) {
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
        return (rep, bands)
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

    @Test func theMarksReadInEveryStateOfTheirRow() async throws {
        _ = NSApplication.shared
        let restore = Self.reduceTransparency()
        defer { restore() }
        let out = ProcessInfo.processInfo.environment["SIDEBAR_DRAWING_OUT"]
        var report: [String] = []
        for dark in [true, false] {
            for state in RowState.allCases {
                let (on, bands) = try await Self.picture(dark: dark, state: state, marks: true)
                let (off, _) = try await Self.picture(dark: dark, state: state, marks: false)
                let name = "\(state.rawValue)-\(dark ? "dark" : "light")"
                if let out {
                    try on.representation(using: .png, properties: [:])?.write(to: URL(fileURLWithPath: "\(out)/sidebar-\(name).png"))
                }
                let a = Pixels(rep: on), b = Pixels(rep: off)
                // The table's rows: Chats, tag, unread, Places, flag, star.
                try #require(bands.count == 6, "\(name): \(bands.count) rows")
                for (row, band) in zip(Self.rows, [bands[1], bands[2], bands[4], bands[5]]) {
                    guard let box = Self.differing(a, b, in: band) else {
                        Issue.record("\(name): the \(row) mark cannot be seen at all")
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
                        // The word against its quiet capsule (the capsule itself is meant to be faint).
                        #expect(text >= 60, "\(name): the \"agents\" tag's word does not read on its capsule (\(Int(text)))")
                    case "unread":
                        #expect(text >= 60, "\(name): the unread number does not read on its capsule (\(Int(text)))")
                        #expect(Self.distance(fill, row0) >= 60, "\(name): the unread capsule melts into its row (\(Int(Self.distance(fill, row0))))")
                    default:
                        #expect(mark >= 60, "\(name): the \(row) mark melts into its row (\(Int(mark)))")
                    }
                }
            }
        }
        if let out { try report.joined(separator: "\n").write(toFile: "\(out)/sidebar-contrast.txt", atomically: true, encoding: .utf8) }
    }

    /// The vibrant layers' filters as the sidebar has them (in every state), composited by VibrancyPass and by Core
    /// Animation's own renderer over the same backdrops: the same colours. (The window server composites with
    /// Core Animation; an in-process drawing has no vibrancy at all.)
    @Test func vibrancyPassCompositesAsCoreAnimationDoes() async throws {
        _ = NSApplication.shared
        guard let device = MTLCreateSystemDefaultDevice(), let queue = device.makeCommandQueue() else { return }
        let restore = Self.reduceTransparency()
        defer { restore() }
        var filters: [String: (filter: NSObject, content: Bool, matrix: [Float])] = [:]
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
                for _ in 0..<6 { spinRunLoop(0.02); w.contentView?.superview?.layoutSubtreeIfNeeded() }
                let views = Self.all(try #require(w.contentView?.superview))
                if state != .unselected { selection.rows = Set(Self.rows) }
                w.makeFirstResponder(state == .grey ? views.first { ($0 as? NSTextField)?.isEditable == true } : views.first { $0 is NSTableView })
                for _ in 0..<6 { spinRunLoop(0.02); w.contentView?.superview?.layoutSubtreeIfNeeded() }
                func walk(_ l: CALayer) {
                    let all: [(Any, Bool)] = [l.compositingFilter].compactMap { $0 }.map { ($0, false) } + (l.filters ?? []).map { ($0, true) }
                    for (f, content) in all {
                        guard let f = f as? NSObject, !(f is NSString), "\(f)".contains("ColorMatrix"),
                              let value = f.value(forKey: "inputColorMatrix") as? NSValue else { continue }
                        var m = [Float](repeating: 0, count: 20)
                        m.withUnsafeMutableBytes { value.getValue($0.baseAddress!, size: 80) }
                        filters["\(f) \(content) \(m)"] = (f, content, m)
                    }
                    for s in l.sublayers ?? [] { walk(s) }
                }
                if let root = w.contentView?.superview?.layer { walk(root) }
                w.contentViewController = nil
                w.close()
            }
        }
        #expect(filters.count >= 3, "the sidebar's vibrant filters were not found (\(filters.count))")
        let backdrops: [SIMD3<Float>] = [[0, 0, 0], [28, 28, 28], [48, 48, 48], [127, 127, 127], [236, 236, 236], [255, 255, 255], [0, 122, 255]]
        for (name, f) in filters {
            for backdrop in backdrops {
                for coverage: Float in [1, 0.5] {
                    let core = try #require(Self.coreAnimation(device: device, queue: queue, backdrop: backdrop, coverage: coverage,
                                                               filter: f.filter, content: f.content))
                    var px = (UInt8(backdrop.x), UInt8(backdrop.y), UInt8(backdrop.z), UInt8(255))
                    VibrancyPass.blend(&px, coverage: coverage, matrix: f.matrix)
                    let ours = SIMD3(Float(px.0), Float(px.1), Float(px.2))
                    // Where a layer covers a pixel fully (a letter's body, a selected row) the same colour; at a
                    // letter's soft edge Core Animation clamps the plain "+bias" matrices before it mixes and the
                    // others after, which the pass does not copy: within a tenth.
                    let tolerance: Float = coverage == 1 ? 6 : 26
                    #expect(Self.distance(ours, core) <= tolerance, "\(name) over \(backdrop) at \(coverage): VibrancyPass \(ours), Core Animation \(core)")
                }
            }
        }
    }

    /// Core Animation's renderer (CARenderer, the compositing code of the window server, in this process): a backdrop
    /// and over it a square of black with `coverage` alpha carrying `filter`. The square's colour.
    static func coreAnimation(device: any MTLDevice, queue: any MTLCommandQueue, backdrop: SIMD3<Float>, coverage: Float,
                              filter: NSObject, content: Bool) -> SIMD3<Float>? {
        let size = 8
        let d = MTLTextureDescriptor.texture2DDescriptor(pixelFormat: .rgba8Unorm, width: size, height: size, mipmapped: false)
        d.usage = [.renderTarget, .shaderRead, .shaderWrite]
        d.storageMode = .shared
        guard let texture = device.makeTexture(descriptor: d), let space = CGColorSpace(name: CGColorSpace.sRGB) else { return nil }
        let renderer = CARenderer(mtlTexture: texture, options: [kCARendererColorSpace as String: space,
                                                                 kCARendererMetalCommandQueue as String: queue])
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        let root = CALayer()
        root.frame = CGRect(x: 0, y: 0, width: size, height: size)
        root.backgroundColor = CGColor(colorSpace: space, components: [CGFloat(backdrop.x / 255), CGFloat(backdrop.y / 255), CGFloat(backdrop.z / 255), 1])
        let square = CALayer()
        square.frame = root.bounds
        square.backgroundColor = CGColor(colorSpace: space, components: [0, 0, 0, CGFloat(coverage)])
        if content { square.filters = [filter] } else { square.compositingFilter = filter }
        root.addSublayer(square)
        renderer.layer = root
        renderer.bounds = root.bounds
        CATransaction.commit()
        CATransaction.flush()
        renderer.beginFrame(atTime: CACurrentMediaTime(), timeStamp: nil)
        renderer.addUpdate(renderer.bounds)
        renderer.render()
        renderer.endFrame()
        guard let done = queue.makeCommandBuffer() else { return nil }
        done.commit()
        done.waitUntilCompleted()
        var px = [UInt8](repeating: 0, count: size * size * 4)
        let i = (size / 2 * size + size / 2) * 4
        // The renderer's work reaches the texture on the run loop's turn.
        for _ in 0..<50 {
            texture.getBytes(&px, bytesPerRow: size * 4, from: MTLRegionMake2D(0, 0, size, size), mipmapLevel: 0)
            if px[i + 3] > 0 { return SIMD3(Float(px[i]), Float(px[i + 1]), Float(px[i + 2])) }
            spinRunLoop(0.01)
        }
        return nil
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
