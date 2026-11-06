import AppKit
import SwiftUI

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

    /// Writes `proxy-shield-sheet.png`: the proxy icon (ProxyShield.swift) in each state, a row each, at 16, 24 and
    /// 32 pt on the light and the dark window background, each drawn at 2x and magnified 4x without smoothing, and
    /// `proxy-shield-<state>-<light|dark>-<size>.png` of each at 2x. docs/proxy-shield.md shows them.
    func renderProxyShields() {
        let scale = 2, magnify: CGFloat = 4, pad: CGFloat = 4
        let sizes: [CGFloat] = [16, 24, 32]
        let looks: [(name: String, appearance: NSAppearance)] = [("light", NSAppearance(named: .aqua)!), ("dark", NSAppearance(named: .darkAqua)!)]
        let labelW: CGFloat = 150, headH: CGFloat = 60, gap: CGFloat = 10, groupGap: CGFloat = 30
        let groupW = sizes.map { ($0 + 2 * pad) * CGFloat(scale) * magnify + gap }.reduce(0, +)
        let rowH = (sizes.max()! + 2 * pad) * CGFloat(scale) * magnify + gap
        let sheetSize = NSSize(width: labelW + CGFloat(looks.count) * (groupW + groupGap),
                               height: headH + CGFloat(ProxyIcon.allCases.count) * rowH + 20)
        guard let sheet = Self.bitmap(sheetSize, scale: 1) else { return }
        var count = 0
        Self.draw(into: sheet) {
            NSColor(white: 0.45, alpha: 1).setFill()
            NSRect(origin: .zero, size: sheetSize).fill()
            for (i, look) in looks.enumerated() {
                Self.text("\(look.name)  16 · 24 · 32 pt", at: NSPoint(x: labelW + CGFloat(i) * (groupW + groupGap), y: sheetSize.height - headH + 14))
            }
            for (r, state) in ProxyIcon.allCases.enumerated() {
                let y = sheetSize.height - headH - CGFloat(r + 1) * rowH
                Self.text("\(state)", at: NSPoint(x: 14, y: y + rowH / 2 - 12))
                for (i, look) in looks.enumerated() {
                    var x = labelW + CGFloat(i) * (groupW + groupGap)
                    for size in sizes {
                        guard let rep = Self.proxyShield(state, size: size, pad: pad, appearance: look.appearance, scale: scale) else { continue }
                        if let png = rep.representation(using: .png, properties: [:]) {
                            try? png.write(to: URL(fileURLWithPath: dir).appendingPathComponent("proxy-shield-\(state)-\(look.name)-\(Int(size)).png"))
                            count += 1
                        }
                        let w = CGFloat(rep.pixelsWide) * magnify
                        NSGraphicsContext.current?.imageInterpolation = .none
                        rep.draw(in: NSRect(x: x, y: y + (rowH - gap - w) / 2, width: w, height: w), from: .zero, operation: .sourceOver,
                                 fraction: 1, respectFlipped: false, hints: [.interpolation: NSNumber(value: NSImageInterpolation.none.rawValue)])
                        x += w + gap
                    }
                }
            }
        }
        if let png = sheet.representation(using: .png, properties: [:]) {
            try? png.write(to: URL(fileURLWithPath: dir).appendingPathComponent("proxy-shield-sheet.png"))
            print("wrote proxy-shield-sheet.png (\(sheet.pixelsWide)x\(sheet.pixelsHigh)) and \(count) icons")
        }
    }

    /// The proxy icon in place, through its SwiftUI view: beside a page title at 28 pt and in rows of body text at 16 pt,
    /// in a real (offscreen) window: `proxy-shield-rows-<dark|light>.png`.
    func renderProxyShieldRows() async {
        for dark in [true, false] {
            await render("proxy-shield-rows-\(dark ? "dark" : "light")", size: NSSize(width: 380, height: 300), dark: dark, settings: true) {
                ProxyShieldPreview()
            }
        }
    }

    /// The proxy icon of `size` points with `pad` around it on the window background of `appearance`, at `scale`, drawn
    /// through its NSImage (whose colours resolve in the appearance it is drawn in, as in the app).
    static func proxyShield(_ state: ProxyIcon, size: CGFloat, pad: CGFloat, appearance: NSAppearance, scale: Int) -> NSBitmapImageRep? {
        let box = NSSize(width: size + 2 * pad, height: size + 2 * pad)
        guard let rep = bitmap(box, scale: scale) else { return nil }
        draw(into: rep) {
            appearance.performAsCurrentDrawingAppearance {
                NSColor.windowBackgroundColor.setFill()
                NSRect(origin: .zero, size: box).fill()
                ProxyShieldDrawing.image(state, size: size).draw(in: NSRect(x: pad, y: pad, width: size, height: size))
            }
        }
        return rep
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

/// The proxy icon in place for the snapshots (debug only): a page title with the 28-pt icon, then a row per state with
/// the 16-pt icon, spaced as the app's rows are (Theme).
private struct ProxyShieldPreview: View {
    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Space.m) {
            HStack(spacing: Theme.Space.s) {
                ProxyShield(state: .on, size: 28)
                Text("Proxy").font(Theme.title)
            }
            ForEach(ProxyIcon.allCases, id: \.self) { state in
                HStack(spacing: Theme.Space.s) {
                    ProxyShield(state: state)
                    Text(verbatim: "\(state)").font(Theme.body)
                    Spacer()
                }
            }
        }
        .padding(Theme.Space.xl)
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        .background(.background)
    }
}
