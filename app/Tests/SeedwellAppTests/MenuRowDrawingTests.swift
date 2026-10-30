import AppKit
import Testing
@testable import SeedwellApp

/// The menu bar's torrent row draws its progress ring (a template image) in the row's ink, as NSMenu draws an item's
/// image: light in a dark menu, white on the hover highlight. Drawn as it is, a template is black.
@MainActor @Suite struct MenuRowDrawingTests {
    /// The brightest opaque pixel of the ring's column, 0 (black) to 1 (white).
    static func ringBrightness(dark: Bool, hovered: Bool) -> CGFloat {
        let item = NSMenuItem(title: "ubuntu.iso", action: nil, keyEquivalent: "")
        item.image = Drawing.ring(0.9)
        let row = MenuTorrentRow()
        row.sync(from: item)
        row.hovered = hovered
        let appearance = NSAppearance(named: dark ? .darkAqua : .aqua)!
        row.appearance = appearance
        let b = row.bounds
        guard let rep = row.bitmapImageRepForCachingDisplay(in: b) else { return -1 }
        appearance.performAsCurrentDrawingAppearance { row.cacheDisplay(in: b, to: rep) }
        let scale = CGFloat(rep.pixelsWide) / b.width
        var best: CGFloat = 0
        // The ring: 16 pt wide, 28 pt in from the row's 5 pt edge (no checkbox), centred vertically.
        for x in Int(34 * scale)..<Int(48 * scale) {
            for y in Int(13 * scale)..<Int(27 * scale) {
                guard let c = rep.colorAt(x: x, y: y)?.usingColorSpace(.sRGB), c.alphaComponent > 0.5 else { continue }
                if hovered, c.blueComponent > c.redComponent + 0.2 { continue }  // the accent highlight behind it
                best = max(best, min(c.redComponent, c.greenComponent, c.blueComponent) / c.alphaComponent)
            }
        }
        return best
    }

    @Test func theRingIsLightInADarkMenu() {
        #expect(Self.ringBrightness(dark: true, hovered: false) > 0.6)
    }

    @Test func theRingIsWhiteOnTheHighlight() {
        #expect(Self.ringBrightness(dark: false, hovered: true) > 0.8)
    }

    @Test func theRingIsDarkInALightMenu() {
        let b = Self.ringBrightness(dark: false, hovered: false)
        #expect(b >= 0 && b < 0.4)
    }
}
