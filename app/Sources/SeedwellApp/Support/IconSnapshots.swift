import AppKit

/// Debug-only visual QA (`--render-snapshots <dir>`, see SnapshotRenderer): the menu bar icon in each of its looks,
/// without and with the attention dot (an agent waiting for an answer), on light, dark and tinted menu bars, each at 2x
/// as the menu bar draws it, and a contact sheet of them all magnified so the dot's pixels can be judged.
extension SnapshotRenderer {
    /// One look of the icon, as the core's `IconLook` gives it.
    private struct Look {
        let name: String
        let down: Double, up: Double
        var progress: Double?
        var moving: Double?
        var problem = false
    }

    /// A menu bar to draw on: its colour and the appearance the glyph's dynamic colours resolve in.
    private struct Bar {
        let name: String
        let color: NSColor
        let dark: Bool
    }

    private static let looks = [
        Look(name: "idle", down: 0, up: 0),
        Look(name: "down", down: 1, up: 0, progress: 0.4),
        Look(name: "up", down: 0, up: 0.5),
        Look(name: "both", down: 0.625, up: 1, progress: 0.8),
        Look(name: "moving", down: 0, up: 0, moving: 0.6),
        Look(name: "problem", down: 0, up: 0, problem: true),
        Look(name: "both-problem", down: 0.625, up: 1, progress: 0.8, problem: true),
    ]

    /// The light and dark menu bars, and a transparent one over a tinted wallpaper in each appearance.
    private static let bars = [
        Bar(name: "light", color: NSColor(srgbRed: 0.93, green: 0.93, blue: 0.94, alpha: 1), dark: false),
        Bar(name: "dark", color: NSColor(srgbRed: 0.15, green: 0.15, blue: 0.16, alpha: 1), dark: true),
        Bar(name: "tinted-light", color: NSColor(srgbRed: 0.70, green: 0.80, blue: 0.93, alpha: 1), dark: false),
        Bar(name: "tinted-dark", color: NSColor(srgbRed: 0.20, green: 0.16, blue: 0.30, alpha: 1), dark: true),
    ]

    /// Writes `menubar-icon-<look>-<plain|dot>-<bar>.png` (the icon on a strip of menu bar, at 2x) and
    /// `menubar-icons-sheet.png`: a row per look, a column per bar and dot, each cell magnified 6x without smoothing.
    func renderMenuBarIcons() {
        let scale = 2, magnify = 6
        let icon = NSSize(width: 20, height: 18)
        let strip = NSSize(width: 34, height: 24)  // the icon with the button's margins, as one menu bar item
        let cell = NSSize(width: strip.width * CGFloat(scale * magnify), height: strip.height * CGFloat(scale * magnify))
        let labelW: CGFloat = 220, headH: CGFloat = 70, gap: CGFloat = 12
        let columns = Self.bars.flatMap { bar in [false, true].map { (bar, $0) } }
        let sheetSize = NSSize(width: labelW + CGFloat(columns.count) * (cell.width + gap),
                               height: headH + CGFloat(Self.looks.count) * (cell.height + gap))
        guard let sheet = Self.bitmap(sheetSize, scale: 1) else { return }
        Self.draw(into: sheet) {
            NSColor(white: 0.5, alpha: 1).setFill()
            NSRect(origin: .zero, size: sheetSize).fill()
            for (c, (bar, dot)) in columns.enumerated() {
                let x = labelW + CGFloat(c) * (cell.width + gap)
                Self.text("\(bar.name)\n\(dot ? "with dot" : "plain")", at: NSPoint(x: x, y: sheetSize.height - headH + 8))
            }
            for (r, look) in Self.looks.enumerated() {
                let y = sheetSize.height - headH - CGFloat(r + 1) * (cell.height + gap) + gap
                Self.text(look.name, at: NSPoint(x: 12, y: y + cell.height / 2 - 12))
                for (c, (bar, dot)) in columns.enumerated() {
                    guard let rep = Self.item(look, bar: bar, dot: dot, icon: icon, strip: strip, scale: scale) else { continue }
                    let name = "menubar-icon-\(look.name)-\(dot ? "dot" : "plain")-\(bar.name)"
                    if let png = rep.representation(using: .png, properties: [:]) {
                        try? png.write(to: URL(fileURLWithPath: dir).appendingPathComponent("\(name).png"))
                    }
                    let x = labelW + CGFloat(c) * (cell.width + gap)
                    NSGraphicsContext.current?.imageInterpolation = .none
                    rep.draw(in: NSRect(x: x, y: y, width: cell.width, height: cell.height), from: .zero, operation: .sourceOver,
                             fraction: 1, respectFlipped: false, hints: [.interpolation: NSNumber(value: NSImageInterpolation.none.rawValue)])
                }
            }
        }
        if let png = sheet.representation(using: .png, properties: [:]) {
            try? png.write(to: URL(fileURLWithPath: dir).appendingPathComponent("menubar-icons-sheet.png"))
            print("wrote menubar-icons-sheet.png (\(sheet.pixelsWide)x\(sheet.pixelsHigh)) and \(Self.looks.count * columns.count) icons")
        }
    }

    /// The icon on a strip of menu bar at `scale`: drawn first on its own clear layer (its cut-out ring clears pixels of
    /// the icon only, as in the menu bar), then onto the bar.
    private static func item(_ look: Look, bar: Bar, dot: Bool, icon: NSSize, strip: NSSize, scale: Int) -> NSBitmapImageRep? {
        guard let layer = bitmap(icon, scale: scale), let out = bitmap(strip, scale: scale) else { return nil }
        let appearance = NSAppearance(named: bar.dark ? .darkAqua : .aqua)!
        draw(into: layer) {
            appearance.performAsCurrentDrawingAppearance {
                let image = Drawing.statusIcon(down: look.down, up: look.up, progress: look.progress, moving: look.moving,
                                               attention: look.problem || dot)
                image.draw(in: NSRect(origin: .zero, size: icon))
            }
        }
        draw(into: out) {
            bar.color.setFill()
            NSRect(origin: .zero, size: strip).fill()
            let origin = NSPoint(x: (strip.width - icon.width) / 2, y: (strip.height - icon.height) / 2)
            layer.draw(in: NSRect(origin: origin, size: icon), from: .zero, operation: .sourceOver, fraction: 1, respectFlipped: false, hints: nil)
        }
        return out
    }

    /// A clear bitmap of `size` points at `scale` pixels per point.
    private static func bitmap(_ size: NSSize, scale: Int) -> NSBitmapImageRep? {
        let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: Int(size.width) * scale, pixelsHigh: Int(size.height) * scale,
                                   bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
                                   colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)
        rep?.size = size
        return rep
    }

    private static func draw(into rep: NSBitmapImageRep, _ body: () -> Void) {
        guard let ctx = NSGraphicsContext(bitmapImageRep: rep) else { return }
        NSGraphicsContext.saveGraphicsState()
        NSGraphicsContext.current = ctx
        body()
        ctx.flushGraphics()
        NSGraphicsContext.restoreGraphicsState()
    }

    private static func text(_ s: String, at p: NSPoint) {
        (s as NSString).draw(at: p, withAttributes: [.font: NSFont.systemFont(ofSize: 20, weight: .medium), .foregroundColor: NSColor.white])
    }
}
