import AppKit
import SwiftUI

/// The proxy's state as its icon shows it is the core's `ProxyIcon` (`ProxyModel.icon`, SPEC §7H): no key (all grey,
/// the logo too, in a lighter tint), off (a green outline, a dark grey logo), on (all green, a white logo), slow (all
/// orange, a white logo), failed (all red, a white logo).
extension ProxyIcon: CaseIterable {
    public static var allCases: [ProxyIcon] { [.noKey, .off, .on, .slow, .failed] }

    /// Drawn as a solid shield (the proxy is in use); otherwise as an outline.
    var filled: Bool { self == .on || self == .slow || self == .failed }
}

/// The proxy icon's drawing: a shield with the proxy logo in it, a path that enters from the left, passes through a
/// relay (the ring) and leaves forward (the arrow). One vector geometry for every size and both platforms:
/// docs/proxy-shield.md copies these numbers for the Windows app, so a change here is a change there.
///
/// The geometry is in a 24 × 24 box, y pointing down (as in SVG and XAML), in path data both can read (absolute
/// `M`, `L`, `C` and `Z` only). The shield is one closed path: an outline is that path stroked; a solid shield is the
/// path filled and stroked with the same colour, so both have the same edge. The logo is stroked lines (round caps and
/// joins) and a stroked circle. The look follows the app's own marks: SF Symbols' `shield` proportions and their
/// regular weight (2 of 24 units, 1.33 pt at 16 pt, as the menu bar arrows' 1.6 of 18), and the green-with-white
/// mark of `Drawing.check()`.
enum ProxyShieldDrawing {
    /// The box the geometry is drawn in.
    static let box: CGFloat = 24
    /// The shield's centre line: a peaked top with rounded corners, straight sides, curving to a soft point.
    static let shieldPath = "M 10.6,2.7 C 11.5,2.35 12.5,2.35 13.4,2.7 L 19.3,4.55 C 20.2,4.85 20.6,5.2 20.6,5.9 L 20.6,11 C 20.6,15.9 17,19.3 13.1,21.25 C 12.4,21.65 11.6,21.65 10.9,21.25 C 7,19.3 3.4,15.9 3.4,11 L 3.4,5.9 C 3.4,5.2 3.8,4.85 4.7,4.55 Z"
    /// The shield's line width.
    static let shieldWidth: CGFloat = 2
    /// The logo's lines: in from the left to the ring, out of the ring to the arrow's tip, and the arrow's head.
    static let logoPath = "M 6.7,11 L 8.95,11 M 12.85,11 L 17.1,11 M 14.8,8.7 L 17.1,11 L 14.8,13.3"
    /// The logo's relay: a circle stroked at `logoWidth`; the lines end on its centre line, so they join it.
    static let ring = (center: CGPoint(x: 10.9, y: 11), radius: CGFloat(1.95))
    /// The logo's line width.
    static let logoWidth: CGFloat = 1.9

    /// The shield's colour: its outline, and its fill when `filled`. System colours, so they follow light and dark mode
    /// and Increase Contrast.
    static func shieldColor(_ state: ProxyIcon) -> NSColor {
        switch state {
        case .noKey: .systemGray
        case .off, .on: .systemGreen
        case .slow: .systemOrange
        case .failed: .systemRed
        }
    }

    /// The hover: no button chrome (no background, border or bezel), only a soft glow around the shield in its own
    /// colour (`shieldColor`: green off and on, orange slow, red failed, grey without a key), fading in and out over
    /// `glowFade`; a press brightens it to `glowPressed`. docs/proxy-shield.md gives Windows the same numbers.
    enum Glow {
        /// The blur radius, in points (the shadow has no offset).
        static let radius: CGFloat = 3.5
        /// The glow's opacity while the pointer is over the shield, and while it is pressed.
        static let hovered: CGFloat = 0.75
        static let pressed: CGFloat = 1
        /// How long it takes to fade in or out.
        static let fade: TimeInterval = 0.15
    }

    /// The glow's colour at `level` (0: none, `Glow.hovered`, `Glow.pressed`): the shield's own tint.
    static func glowColor(_ state: ProxyIcon, level: CGFloat) -> NSColor {
        shieldColor(state).withAlphaComponent(max(0, min(1, level)))
    }

    /// The logo's colour: the shield's grey without a key, white on a solid shield, and dark grey (light grey on a dark
    /// background) inside the green outline.
    static func logoColor(_ state: ProxyIcon) -> NSColor {
        switch state {
        case .noKey: .systemGray
        case .off: offLogo
        case .on, .slow, .failed: .white
        }
    }

    /// "Dark grey" that stays readable: dark grey on a light background, light grey on a dark one, each a step further
    /// with Increase Contrast. The values are from Apple's grey scale (the system greys 2 to 6 of iOS; docs/proxy-shield.md).
    static let offLogo = NSColor(name: "proxyShieldOffLogo") { appearance in
        switch appearance.bestMatch(from: [.aqua, .darkAqua, .accessibilityHighContrastAqua, .accessibilityHighContrastDarkAqua]) {
        case .darkAqua: hex(0xC7C7CC)
        case .accessibilityHighContrastAqua: hex(0x1C1C1E)
        case .accessibilityHighContrastDarkAqua: hex(0xE5E5EA)
        default: hex(0x48484A)
        }
    }

    private static func hex(_ v: UInt32) -> NSColor {
        NSColor(srgbRed: CGFloat((v >> 16) & 255) / 255, green: CGFloat((v >> 8) & 255) / 255, blue: CGFloat(v & 255) / 255, alpha: 1)
    }

    /// The colours `state` draws in `appearance`, resolved (sRGB).
    static func resolved(_ state: ProxyIcon, in appearance: NSAppearance) -> (shield: CGColor, logo: CGColor) {
        var out = (shield: CGColor(gray: 0.5, alpha: 1), logo: CGColor(gray: 0.5, alpha: 1))
        appearance.performAsCurrentDrawingAppearance {
            out = (shieldColor(state).usingColorSpace(.sRGB)?.cgColor ?? out.shield, logoColor(state).usingColorSpace(.sRGB)?.cgColor ?? out.logo)
        }
        return out
    }

    /// The shield and the logo (lines and ring) as paths in the 24-unit box (immutable CGPaths, safe to share).
    nonisolated(unsafe) static let shield = path(shieldPath)
    nonisolated(unsafe) static let logo: CGPath = {
        let p = path(logoPath).mutableCopy() ?? CGMutablePath()
        p.addEllipse(in: CGRect(x: ring.center.x - ring.radius, y: ring.center.y - ring.radius, width: 2 * ring.radius, height: 2 * ring.radius))
        return p
    }()

    /// Draws `state` into a `size` × `size` square at the origin of `ctx`, whose y axis points down. `logo: false`
    /// leaves the logo out (the tests compare the two to find the logo's pixels).
    static func draw(_ state: ProxyIcon, size: CGFloat, colors: (shield: CGColor, logo: CGColor), in ctx: CGContext, logo: Bool = true) {
        let k = size / box
        ctx.saveGState()
        defer { ctx.restoreGState() }
        ctx.scaleBy(x: k, y: k)
        ctx.setLineCap(.round)
        ctx.setLineJoin(.round)
        if state.filled {
            ctx.addPath(shield)
            ctx.setFillColor(colors.shield)
            ctx.fillPath()
        }
        ctx.addPath(shield)
        ctx.setStrokeColor(colors.shield)
        ctx.setLineWidth(shieldWidth)
        ctx.strokePath()
        guard logo else { return }
        ctx.addPath(Self.logo)
        ctx.setStrokeColor(colors.logo)
        ctx.setLineWidth(logoWidth)
        ctx.strokePath()
    }

    /// The icon as an image of `size` points; its colours resolve when it is drawn, in the appearance it is drawn in
    /// (a menu, a light or dark window, Increase Contrast), as `Drawing.statusIcon` does. `logo: false` is for the tests.
    static func image(_ state: ProxyIcon, size: CGFloat = 16, logo: Bool = true) -> NSImage {
        let image = NSImage(size: NSSize(width: size, height: size), flipped: true) { _ in
            guard let ctx = NSGraphicsContext.current?.cgContext else { return false }
            draw(state, size: size, colors: resolved(state, in: NSAppearance.currentDrawing()), in: ctx, logo: logo)
            return true
        }
        image.isTemplate = false
        return image
    }

    /// A CGPath from absolute `M`, `L`, `C` and `Z` path data (commas or spaces between numbers), one element per
    /// command. Data it cannot read (another command, a missing number) ends the path there; the tests count the
    /// elements, so a typo in the data fails them instead of drawing half a shield.
    static func path(_ data: String) -> CGPath {
        let p = CGMutablePath()
        let tokens = data.split(whereSeparator: { $0 == " " || $0 == "," })
        var i = 0
        func points(_ n: Int) -> [CGPoint]? {
            guard i + 2 * n <= tokens.count else { return nil }
            var out: [CGPoint] = []
            for _ in 0..<n {
                guard let x = Double(tokens[i]), let y = Double(tokens[i + 1]) else { return nil }
                out.append(CGPoint(x: x, y: y))
                i += 2
            }
            return out
        }
        while i < tokens.count {
            let command = tokens[i]
            i += 1
            switch command {
            case "M": guard let q = points(1) else { return p }; p.move(to: q[0])
            case "L": guard let q = points(1) else { return p }; p.addLine(to: q[0])
            case "C": guard let q = points(3) else { return p }; p.addCurve(to: q[2], control1: q[0], control2: q[1])
            case "Z": p.closeSubpath()
            default: return p
            }
        }
        return p
    }
}

/// The proxy icon in SwiftUI, `size` points square: the same paths, drawn with the dynamic colours, which SwiftUI
/// resolves in the view's own colour scheme and contrast (Increase Contrast included; an `NSAppearance` for it cannot be
/// made by name, so the colours are not resolved here).
struct ProxyShield: View {
    let state: ProxyIcon
    var size: CGFloat = 16

    var body: some View {
        let shield = Color(nsColor: ProxyShieldDrawing.shieldColor(state))
        let logo = Color(nsColor: ProxyShieldDrawing.logoColor(state))
        let filled = state.filled
        Canvas { context, canvas in
            let k = min(canvas.width, canvas.height) / ProxyShieldDrawing.box
            context.scaleBy(x: k, y: k)
            let outline = Path(ProxyShieldDrawing.shield)
            if filled { context.fill(outline, with: .color(shield)) }
            context.stroke(outline, with: .color(shield), style: StrokeStyle(lineWidth: ProxyShieldDrawing.shieldWidth, lineCap: .round, lineJoin: .round))
            context.stroke(Path(ProxyShieldDrawing.logo), with: .color(logo),
                           style: StrokeStyle(lineWidth: ProxyShieldDrawing.logoWidth, lineCap: .round, lineJoin: .round))
        }
        .frame(width: size, height: size)
    }
}
