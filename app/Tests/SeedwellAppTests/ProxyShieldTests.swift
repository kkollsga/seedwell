import AppKit
import Testing
@testable import SeedwellApp

/// The proxy icon (ProxyShield.swift), judged on its rendered pixels in light and dark mode, through the NSImage the
/// app draws. Each test names the failure it catches; none pins exact pixels or geometry.
@MainActor @Suite(.timeLimit(.minutes(1))) struct ProxyShieldTests {
    struct RGB {
        var r, g, b: Double

        /// WCAG relative luminance.
        var luminance: Double {
            func lin(_ c: Double) -> Double { c <= 0.04045 ? c / 12.92 : pow((c + 0.055) / 1.055, 2.4) }
            return 0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b)
        }

        func contrast(_ o: RGB) -> Double {
            let a = luminance, b = o.luminance
            return (max(a, b) + 0.05) / (min(a, b) + 0.05)
        }

        func distance(_ o: RGB) -> Double { max(abs(r - o.r), abs(g - o.g), abs(b - o.b)) }
    }

    /// The icon at 32 pt and 2x over the window background of `dark` or light, as sRGB pixels; without its logo when
    /// `logo` is false.
    static func pixels(_ state: ProxyIcon, dark: Bool, logo: Bool = true) -> [RGB] {
        let size: CGFloat = 32, scale: CGFloat = 2, px = Int(size * scale)
        let ctx = CGContext(data: nil, width: px, height: px, bitsPerComponent: 8, bytesPerRow: px * 4,
                            space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
        ctx.scaleBy(x: scale, y: scale)
        NSGraphicsContext.saveGraphicsState()
        NSGraphicsContext.current = NSGraphicsContext(cgContext: ctx, flipped: false)
        NSAppearance(named: dark ? .darkAqua : .aqua)!.performAsCurrentDrawingAppearance {
            NSColor.windowBackgroundColor.setFill()
            NSRect(x: 0, y: 0, width: size, height: size).fill()
            ProxyShieldDrawing.image(state, size: size, logo: logo).draw(in: NSRect(x: 0, y: 0, width: size, height: size))
        }
        NSGraphicsContext.restoreGraphicsState()
        let data = ctx.data!.assumingMemoryBound(to: UInt8.self)
        return (0..<(px * px)).map { i in RGB(r: Double(data[i * 4]) / 255, g: Double(data[i * 4 + 1]) / 255, b: Double(data[i * 4 + 2]) / 255) }
    }

    /// What one state draws: the background, the shield's pixels (those that differ from the background without the
    /// logo) and the mean colour of their core, and the logo's pixels (those that change when the logo is drawn), each with the
    /// colour it has and the colour behind it.
    @MainActor struct Drawn {
        let background: RGB
        let shieldCount: Int
        let shield: RGB
        let logo: [(drawn: RGB, behind: RGB)]

        init(_ state: ProxyIcon, dark: Bool) {
            let with = ProxyShieldTests.pixels(state, dark: dark), without = ProxyShieldTests.pixels(state, dark: dark, logo: false)
            background = without[0]
            let shieldPixels = without.filter { $0.distance(without[0]) > 0.25 }
            shieldCount = shieldPixels.count
            // The solid core: the half most unlike the background (not the outline's smoothed edges).
            let core = shieldPixels.sorted { $0.distance(without[0]) > $1.distance(without[0]) }.prefix(max(1, shieldPixels.count / 2))
            let n = Double(max(1, core.count))
            shield = RGB(r: core.map(\.r).reduce(0, +) / n, g: core.map(\.g).reduce(0, +) / n, b: core.map(\.b).reduce(0, +) / n)
            logo = zip(with, without).filter { $0.0.distance($0.1) > 0.1 }.map { (drawn: $0.0, behind: $0.1) }
        }

        /// The logo's own colour: the mean of its most changed half (its solid core, not the smoothed edge).
        var logoColor: RGB {
            let core = logo.sorted { $0.drawn.distance($0.behind) > $1.drawn.distance($1.behind) }.prefix(max(1, logo.count / 2))
            let n = Double(core.count)
            return RGB(r: core.map(\.drawn.r).reduce(0, +) / n, g: core.map(\.drawn.g).reduce(0, +) / n, b: core.map(\.drawn.b).reduce(0, +) / n)
        }

        /// The contrast of the logo with what is behind it, at its solid core (the 75th percentile of its pixels).
        var logoContrast: Double {
            let c = logo.map { $0.drawn.contrast($0.behind) }.sorted()
            return c.isEmpty ? 1 : c[c.count * 3 / 4]
        }
    }

    /// Catches two states drawn alike, or a state given another's colours: Slow drawn green, Failed orange, Off filled
    /// like On, No key tinted. Each state differs from every other in its shield's or its logo's colour, the shield
    /// has its state's hue, and only the states in use draw a solid shield.
    @Test func everyStateDrawsItsOwnColours() {
        for dark in [false, true] {
            let drawn = ProxyIcon.allCases.map { ($0, Drawn($0, dark: dark)) }
            for (i, (a, da)) in drawn.enumerated() {
                for (b, db) in drawn[(i + 1)...] {
                    #expect(da.shield.distance(db.shield) > 0.15 || da.logoColor.distance(db.logoColor) > 0.15,
                            "\(a) and \(b) look alike (dark: \(dark))")
                }
                let s = da.shield
                let saturation = max(s.r, s.g, s.b) - min(s.r, s.g, s.b)
                switch a {
                case .noKey: #expect(saturation < 0.08, "no key is grey: \(s) (dark: \(dark))")
                case .off, .on: #expect(s.g > s.r + 0.3 && s.g > s.b + 0.3, "\(a) is green: \(s) (dark: \(dark))")
                case .slow: #expect(s.r > 0.85 && s.g > 0.4 && s.g < 0.75 && s.b < 0.35, "slow is orange: \(s) (dark: \(dark))")
                case .failed: #expect(s.r > 0.85 && s.g < 0.4 && s.b < 0.4, "failed is red: \(s) (dark: \(dark))")
                }
            }
            // In use (On, Slow, Failed): solid; No key and Off: an outline, about a third of the pixels.
            let inUse: Set<ProxyIcon> = [.on, .slow, .failed]
            let outline = drawn.filter { !inUse.contains($0.0) }.map(\.1.shieldCount).max() ?? 0
            for (state, d) in drawn where inUse.contains(state) {
                #expect(d.shieldCount > outline * 2, "\(state) is solid, the outlines are not (dark: \(dark))")
            }
        }
    }

    /// Catches a logo that cannot be seen: drawn in its shield's own colour (no pixel changes), dark grey on the dark
    /// window, or white on a light background. Inside an outline the logo stands out from the window by 3:1 (WCAG's
    /// non-text contrast); on a solid shield, white keeps at least 1.8:1 on the system colour (macOS's own white-on-green
    /// marks; Increase Contrast deepens the system colours further). And the shield stands out from the window by 2:1.
    @Test func theLogoStandsOutFromWhatIsBehindIt() {
        for dark in [false, true] {
            for state in ProxyIcon.allCases {
                let d = Drawn(state, dark: dark)
                #expect(d.logo.count > 60, "\(state)'s logo draws pixels (dark: \(dark)): \(d.logo.count)")
                #expect(d.logoContrast >= (state.filled ? 1.8 : 3), "\(state)'s logo contrast \(d.logoContrast) (dark: \(dark))")
                #expect(d.shield.contrast(d.background) >= 2, "\(state)'s shield on the window: \(d.shield.contrast(d.background)) (dark: \(dark))")
            }
        }
    }

    /// Catches a geometry the drawing cannot read (a typo in the path data shared with docs/proxy-shield.md drops a
    /// segment or the whole shape): every command of the data becomes one element of the path, the shield fills most
    /// of its box, centred, and the logo lies inside it.
    @Test func thePathDataDrawsAWholeShieldWithTheLogoInside() {
        for data in [ProxyShieldDrawing.shieldPath, ProxyShieldDrawing.logoPath] {
            var elements = 0
            ProxyShieldDrawing.path(data).applyWithBlock { _ in elements += 1 }
            #expect(elements == data.filter(\.isLetter).count, "\(data)")
        }
        let shield = ProxyShieldDrawing.shield.boundingBoxOfPath, logo = ProxyShieldDrawing.logo.boundingBoxOfPath
        #expect(shield.width > 15 && shield.height > 18, "\(shield)")
        #expect(abs(shield.midX - ProxyShieldDrawing.box / 2) < 0.5 && abs(shield.midY - ProxyShieldDrawing.box / 2) < 1, "\(shield)")
        #expect(shield.insetBy(dx: ProxyShieldDrawing.shieldWidth, dy: ProxyShieldDrawing.shieldWidth).contains(logo), "\(logo) in \(shield)")
        #expect(logo.width > 8, "\(logo)")
    }
}
